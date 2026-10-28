//! Markdown rendering for the transcript's left panel.
//!
//! Pure functions: markdown text in, styled [`Line`]s out. No I/O, no
//! terminal access - fully unit-testable.
//!
//! GFM 0.29 implementation (https://github.github.com/gfm/), adapted to
//! a terminal transcript:
//!
//! Blocks: ATX headings (incl. empty headings, closing hashes, inline
//! formatting), setext headings (multi-line text, inline formatting),
//! indented code blocks, fenced code blocks (``` and ~~~, fence-length
//! and info-string rules), HTML blocks (types 1-7, rendered literally),
//! link reference definitions (full/collapsed/shortcut, titles, first
//! definition wins), tables (header/delimiter alignment, cell inline
//! formatting, escaped pipes), block quotes (nested, lazy continuation,
//! other blocks inside), lists (bullet/ordered, `.` and `)` markers,
//! start numbers, loose/tight, continuation lines, nested lists, task
//! lists), thematic breaks, paragraphs.
//!
//! Inlines: backslash escapes, entity/numeric references, code spans
//! (multi-backtick runs, space-stripping rule), emphasis/strong emphasis
//! (delimiter runs, left/right flanking, can-open/can-close, the
//! odd-match rule of 3, nesting, rules 9-17), strikethrough, links
//! (inline with titles and balanced destinations, reference
//! full/collapsed/shortcut), images (inline/reference, alt as plain
//! text), autolinks (<scheme:...>, <email>), autolink extension
//! (www., bare email, bare scheme URLs with balanced-paren and
//! trailing-punctuation rules), raw HTML (literal), disallowed raw HTML
//! (blocklist tags literal, never parsed as markup).
//!
//! Terminal adaptations (deliberate, not spec bugs):
//! - No HTML output: heading levels all use the heading style, link
//!   destinations and titles are dropped (only the link text is shown,
//!   in primary), image URLs are dropped (only alt text, primary),
//!   horizontal rules render as a dim `· · ·` separator.
//! - Raw HTML other than autolinks is left literal on purpose:
//!   stripping `<…>` would mangle generics like `Vec<T>`.
//! - Soft and hard line breaks both render as line breaks: a paragraph's
//!   input lines are kept as separate output lines (not joined and
//!   re-flowed), so intentional line breaks in a message survive.
//!   A trailing backslash or two trailing spaces therefore needs no
//!   special output — the line break is already there.
//! - Pantheon extensions kept from before: bare section headers (a
//!   short plain line between blanks becomes a heading) and tag lines
//!   (`+ Thought · 8.9s`); fenced/indented code bodies are indented
//!   two spaces in theme code; tables drop pipes/rule lines and align
//!   columns with two-space gaps (no box-drawing anywhere).
//! - Wrapping: prose, headings, blockquotes and list bodies wrap at
//!   the transcript width (see `render_markdown_wrapped`); fenced
//!   code, indented code and tables never wrap.
//!
//! All styling uses semantic [`Theme`] fields only; no literal colors
//! appear here.

use std::collections::HashMap;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::session::theme::Theme;

/// Maximum length (chars) of a bare section header or a tag line.
const SHORT_LINE: usize = 48;

/// Tags whose raw HTML is disallowed by the GFM extension (6.11).
/// They are rendered literally, never as markup, and their content
/// inside an HTML block is not parsed as markdown either.
const DISALLOWED_TAGS: [&str; 9] = [
    "title", "textarea", "style", "xmp", "iframe", "noembed", "noframes", "script", "plaintext",
];

/// Block tags for HTML blocks (types 1, 6, 7), GFM 4.6.
const BLOCK_TAGS: [&str; 62] = [
    "address", "article", "aside", "base", "basefont", "blockquote", "body", "caption", "center",
    "col", "colgroup", "dd", "details", "dialog", "dir", "div", "dl", "dt", "fieldset",
    "figcaption", "figure", "footer", "form", "frame", "frameset", "h1", "h2", "h3", "h4", "h5",
    "h6", "head", "header", "hr", "html", "iframe", "legend", "li", "link", "main", "menu",
    "menuitem", "nav", "noframes", "ol", "optgroup", "option", "p", "param", "search",
    "section", "summary", "table", "tbody", "td", "tfoot", "th", "thead", "title", "tr",
    "track", "ul",
];

/// Render markdown `text` to styled lines (no trailing blank line).
pub fn render_markdown(text: &str, th: &Theme) -> Vec<Line<'static>> {
    render_markdown_at(text, th, None)
}

/// Render markdown `text`, wrapping prose lines to `width` columns.
/// Fenced code blocks, indented code blocks and tables are never
/// wrapped (wrapping them would mangle their layout); prose,
/// headings, blockquotes, and list bodies are wrapped at word
/// boundaries, with styled spans preserved and continuation lines
/// indented to the body column. Wrapping happens here, before the
/// transcript builds its line list, so scroll offsets, jump targets,
/// click-to-toggle rows, and image placements all stay in wrapped
/// coordinates — a ratatui `Paragraph::wrap()` on the whole transcript
/// would break those, because the transcript measures scroll in
/// unwrapped lines.
pub fn render_markdown_wrapped(text: &str, th: &Theme, width: usize) -> Vec<Line<'static>> {
    render_markdown_at(text, th, Some(width))
}

// ---------------------------------------------------------------------------
// Block parsing
// ---------------------------------------------------------------------------

/// A link reference definition: destination only (titles are dropped
/// in the terminal rendering, so they are parsed but not stored).
type Defs = HashMap<String, String>;

fn render_markdown_at(text: &str, th: &Theme, width: Option<usize>) -> Vec<Line<'static>> {
    // Tabs expand to the next multiple of 4 everywhere (GFM 2.2:
    // expansion happens before block parsing, so interior tabs are
    // not literal content either).
    let expanded = expand_tabs(text);
    let lines: Vec<String> = expanded.lines().map(|s| s.to_string()).collect();
    let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    let defs = collect_defs(&refs);
    let mut r = Renderer {
        th,
        defs: &defs,
        width,
        last_was_paragraph: false,
    };
    let mut out = r.render_blocks(&refs);
    while out.last().is_some_and(|l| l.width() == 0) {
        out.pop();
    }
    while out.first().is_some_and(|l| l.width() == 0) {
        out.remove(0);
    }
    out
}

/// Expand every tab to spaces (tab stops of 4, GFM 2.2). Column is
/// counted in chars, matching the previous leading-only behaviour.
fn expand_tabs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut col = 0usize;
        for c in line.chars() {
            if c == '\t' {
                let n = 4 - (col % 4);
                for _ in 0..n {
                    out.push(' ');
                }
                col += n;
            } else {
                out.push(c);
                if c == '\n' {
                    col = 0;
                } else {
                    col += 1;
                }
            }
        }
    }
    out
}

struct Renderer<'a> {
    th: &'a Theme,
    defs: &'a Defs,
    width: Option<usize>,
    /// Whether the last emitted block was a paragraph line. Used for
    /// list-interruption rules (GFM 5.4: an ordered list may only
    /// interrupt a paragraph when it starts at 1; a bullet list may
    /// only interrupt with a non-empty first item).
    last_was_paragraph: bool,
}

impl<'a> Renderer<'a> {
    fn render_blocks(&mut self, lines: &[&str]) -> Vec<Line<'static>> {
        let n = lines.len();
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut i = 0usize;
        // Fence state is tracked across the loop for fenced blocks.
        while i < n {
            let line = lines[i];
            let indent = leading_spaces(line);
            let trimmed = line.trim();

            // --- link reference definitions: consumed, never rendered.
            // Multi-line defs (destination/title on the next line)
            // consume their continuation lines too.
            if indent <= 3 {
                if let Some((label, dest, consumed)) = parse_def_at(lines, i) {
                    let _ = (label, dest); // defs already collected globally
                    self.last_was_paragraph = false;
                    i += consumed;
                    continue;
                }
            }

            // --- blank lines ------------------------------------------------
            if is_blank(line) {
                out.push(Line::default());
                self.last_was_paragraph = false;
                i += 1;
                continue;
            }

            // --- fenced code -------------------------------------------------
            if indent <= 3 {
                if let Some((fence_char, fence_len, _info)) = fence_open(trimmed) {
                    let mut body: Vec<String> = Vec::new();
                    let mut j = i + 1;
                    while j < n {
                        let l = lines[j];
                        let li = leading_spaces(l);
                        if li <= 3 {
                            if let Some((c2, len2)) = fence_close(l.trim_start(), fence_char) {
                                if len2 >= fence_len {
                                    break;
                                }
                            }
                        }
                        body.push(l.to_string());
                        j += 1;
                    }
                    if out.last().is_some_and(|l| l.width() > 0) {
                        out.push(Line::default());
                    }
                    for b in &body {
                        if is_blank(b) {
                            out.push(Line::default());
                        } else {
                            out.push(Line::from(Span::styled(
                                format!("  {b}"),
                                Style::default().fg(self.th.code),
                            )));
                        }
                    }
                    if j + 1 < n && !is_blank(lines[j + 1]) && j < n {
                        // Blank line after the block, only when the next
                        // line is non-blank and a closing fence existed.
                        if j < n {
                            out.push(Line::default());
                        }
                    }
                    self.last_was_paragraph = false;
                    i = if j < n { j + 1 } else { n };
                    continue;
                }
            }

            // --- indented code ----------------------------------------------
            // 4+ spaces, not a paragraph continuation (GFM 4.4: an
            // indented code block cannot interrupt a paragraph).
            if indent >= 4 && !self.last_was_paragraph {
                let mut body: Vec<String> = Vec::new();
                let mut j = i;
                while j < n {
                    let l = lines[j];
                    if is_blank(l) {
                        // A blank line belongs to the block only if an
                        // indented line follows it.
                        let mut k = j + 1;
                        while k < n && is_blank(lines[k]) {
                            k += 1;
                        }
                        if k < n && leading_spaces(lines[k]) >= 4 {
                            body.push(String::new());
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    if leading_spaces(l) >= 4 {
                        body.push(l[4.min(l.len())..].to_string());
                        j += 1;
                    } else {
                        break;
                    }
                }
                if out.last().is_some_and(|l| l.width() > 0) {
                    out.push(Line::default());
                }
                for b in &body {
                    if b.is_empty() {
                        out.push(Line::default());
                    } else {
                        out.push(Line::from(Span::styled(
                            format!("  {b}"),
                            Style::default().fg(self.th.code),
                        )));
                    }
                }
                self.last_was_paragraph = false;
                i = j;
                continue;
            }

            // --- HTML blocks --------------------------------------------------
            if indent <= 3 {
                if let Some(end) = html_block_end(lines, i, self.last_was_paragraph) {
                    for l in &lines[i..end] {
                        out.push(Line::from(Span::styled(
                            l.to_string(),
                            Style::default().fg(self.th.body),
                        )));
                    }
                    self.last_was_paragraph = false;
                    i = end;
                    continue;
                }
            }

            // --- pipe tables --------------------------------------------------
            if is_table_start(lines, i) {
                let (table, next) = self.render_table(lines, i);
                out.extend(table);
                self.last_was_paragraph = false;
                i = next;
                continue;
            }

            // --- setext headings ----------------------------------------------
            // A plain paragraph line (or a run of plain lines) followed
            // by a setext underline. The whole run becomes the heading.
            if indent <= 3 && is_setext_text(trimmed) {
                // Scan the paragraph run for an underline at its end.
                let mut k = i;
                while k + 1 < n {
                    if setext_underline(lines, k).is_some() {
                        break;
                    }
                    let nxt = lines[k + 1];
                    if is_blank(nxt)
                        || leading_spaces(nxt) > 3
                        || !is_setext_text(nxt.trim())
                        || is_table_start(lines, k + 1)
                        || fence_open(nxt.trim()).is_some()
                        || is_hr(nxt.trim())
                        || atx_heading(nxt.trim()).is_some()
                        || list_item(nxt.trim()).is_some()
                        || is_quote_start(nxt.trim())
                    {
                        break;
                    }
                    k += 1;
                }
                if setext_underline(lines, k).is_some() {
                    for h in &lines[i..=k] {
                        let spans = self.heading_spans(h.trim());
                        self.push_wrapped(&mut out, Line::from(spans), 0);
                    }
                    self.last_was_paragraph = false;
                    i = k + 2;
                    continue;
                }
            }

            // --- horizontal rules ----------------------------------------------
            if indent <= 3 && is_hr(trimmed) {
                out.push(Line::from(Span::styled(
                    "· · ·",
                    Style::default().fg(self.th.dim),
                )));
                self.last_was_paragraph = false;
                i += 1;
                continue;
            }

            // --- ATX headings ---------------------------------------------------
            if indent <= 3 {
                if let Some(heading) = atx_heading(trimmed) {
                    if heading.is_empty() {
                        // Empty heading: an empty line in the terminal.
                        out.push(Line::default());
                    } else {
                        let spans = self.heading_spans(heading);
                        self.push_wrapped(&mut out, Line::from(spans), 0);
                    }
                    self.last_was_paragraph = false;
                    i += 1;
                    continue;
                }
            }

            // --- tag lines (`+ Thought · 8.9s`) --------------------------------
            if trimmed.chars().count() <= SHORT_LINE {
                if let Some(rest) = trimmed.strip_prefix("+ ") {
                    out.push(Line::from(vec![
                        Span::styled("+", Style::default().fg(self.th.emphasis).bold()),
                        Span::styled(format!(" {rest}"), Style::default().fg(self.th.dim)),
                    ]));
                    self.last_was_paragraph = false;
                    i += 1;
                    continue;
                }
            }

            // --- blockquotes -----------------------------------------------------
            if indent <= 3 && is_quote_start(trimmed) {
                let (inner, next) = collect_quote(lines, i);
                let inner_width = self.width.map(|w| w.saturating_sub(2));
                let mut sub = Renderer {
                    th: self.th,
                    defs: self.defs,
                    width: inner_width,
                    last_was_paragraph: false,
                };
                let inner_refs: Vec<&str> = inner.iter().map(|s| s.as_str()).collect();
                let inner_lines = sub.render_blocks(&inner_refs);
                for l in inner_lines {
                    if l.width() == 0 {
                        out.push(Line::default());
                    } else {
                        let mut spans = vec![Span::styled(
                            "> ",
                            Style::default().fg(self.th.dim),
                        )];
                        spans.extend(l.spans);
                        out.push(Line::from(spans));
                    }
                }
                self.last_was_paragraph = false;
                i = next;
                continue;
            }

            // --- lists ------------------------------------------------------------
            if indent <= 3 {
                if let Some((marker, _body)) = list_item(trimmed) {
                    // Interruption rules (GFM 5.4).
                    let interrupts = if self.last_was_paragraph {
                        match marker_kind(&marker) {
                            MarkerKind::Bullet(_) => {
                                // Empty first item cannot interrupt.
                                !marker_is_empty_item(trimmed, &marker)
                            }
                            MarkerKind::Ordered(start) => start == 1,
                        }
                    } else {
                        true
                    };
                    if interrupts {
                        let (list_lines, next) = self.render_list(lines, i);
                        out.extend(list_lines);
                        self.last_was_paragraph = false;
                        i = next;
                        continue;
                    }
                }
            }

            // --- bare section headers ----------------------------------------------
            if is_section_header(lines, i) {
                let spans = self.heading_spans(trimmed);
                self.push_wrapped(&mut out, Line::from(spans), 0);
                self.last_was_paragraph = false;
                i += 1;
                continue;
            }

            // --- plain body ----------------------------------------------------------
            // A backtick run unclosed at end of line joins the following
            // lines into one logical paragraph for span parsing (GFM
            // 6.3: a code span may cross a soft break), but the
            // rendered line breaks do not move: the joined spans are
            // split back at the original line boundaries, so layout is
            // exactly the per-line rendering and only the span styling
            // crosses the break. Full paragraph joining would re-flow
            // soft breaks and is deliberately not done.
            if let Some((joined, next)) = self.joined_span_lines(lines, i) {
                for spans in joined {
                    self.push_wrapped(&mut out, Line::from(spans), 0);
                }
                self.last_was_paragraph = true;
                i = next;
            } else {
                let spans = inline(trimmed, self.th, self.defs);
                self.push_wrapped(&mut out, Line::from(spans), 0);
                self.last_was_paragraph = true;
                i += 1;
            }
        }
        out
    }

    /// If line `i` ends with an unclosed backtick run, gather the
    /// following plain-body lines until the run closes and parse the
    /// group as one logical paragraph, split back into one span list
    /// per source line. Returns `None` when line `i` has no unclosed
    /// run, or when the run never closes inside the paragraph — the
    /// caller then renders the lines one by one exactly as before
    /// (backticks literal), so a never-closed span changes nothing.
    fn joined_span_lines(
        &self,
        lines: &[&str],
        i: usize,
    ) -> Option<(Vec<Vec<Span<'static>>>, usize)> {
        let first = lines[i].trim();
        if !unclosed_backtick_run(first) {
            return None;
        }
        let mut group: Vec<&str> = vec![first];
        let mut joined = first.to_string();
        let mut j = i + 1;
        while unclosed_backtick_run(&joined) {
            if j >= lines.len() || !is_plain_body_continuation(lines, j) {
                return None;
            }
            let t = lines[j].trim();
            joined.push('\n');
            joined.push_str(t);
            group.push(t);
            j += 1;
        }
        // Split the parsed spans back at the join boundaries. Every
        // '\n' in span content came from a join (source lines contain
        // none), and it sits inside the code span it belongs to — the
        // join boundary itself renders as the line break, not as a
        // visible space, so it is dropped.
        let spans = inline(&joined, self.th, self.defs);
        let mut per_line: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        for sp in spans {
            let mut rest = sp.content.as_ref();
            while let Some(pos) = rest.find('\n') {
                let (head, tail) = rest.split_at(pos);
                if !head.is_empty() {
                    per_line
                        .last_mut()
                        .expect("per_line never empty")
                        .push(Span::styled(head.to_string(), sp.style));
                }
                per_line.push(Vec::new());
                rest = &tail[1..];
            }
            if !rest.is_empty() {
                per_line
                    .last_mut()
                    .expect("per_line never empty")
                    .push(Span::styled(rest.to_string(), sp.style));
            }
        }
        // Defensive: the split must yield exactly one line per source
        // line; anything else means the join did not behave, and the
        // caller falls back to per-line rendering.
        if per_line.len() != group.len() {
            return None;
        }
        Some((per_line, j))
    }

    /// Heading spans: inline formatting parsed, then recoloured to the
    /// heading style (bold). Code spans keep their code colour.
    fn heading_spans(&self, text: &str) -> Vec<Span<'static>> {
        let spans = inline(text, self.th, self.defs);
        spans
            .into_iter()
            .map(|mut s| {
                // Explicit fg assignment (not patch): in the pantheon
                // theme code and body share a colour, so fg cannot
                // distinguish code spans, and patch precedence is
                // version-dependent.
                s.style.fg = Some(self.th.heading);
                s.style = s.style.add_modifier(Modifier::BOLD);
                s
            })
            .collect()
    }

    fn push_wrapped(&self, out: &mut Vec<Line<'static>>, line: Line<'static>, cont: usize) {
        let Some(w) = self.width else {
            out.push(line);
            return;
        };
        if w == 0 || line.width() <= w {
            out.push(line);
            return;
        }
        out.extend(wrap_line(line, w, cont));
    }

    /// Render a list starting at `i`; returns lines and next index.
    /// Items are grouped by marker kind (bullet char / ordered
    /// delimiter); a change of kind ends the list (GFM 5.4).
    /// Each item is (marker, content_indent, content lines).
    fn render_list(&mut self, lines: &[&str], start: usize) -> (Vec<Line<'static>>, usize) {
        let first_trimmed = lines[start].trim_start();
        let first_marker = list_marker_text(first_trimmed);
        let kind = marker_kind(&first_marker);
        let base_indent = leading_spaces(lines[start]);
        let mut items: Vec<(String, usize, Vec<String>)> = Vec::new();
        let mut loose = false;
        let mut i = start;
        let n = lines.len();
        while i < n {
            let line = lines[i];
            if is_blank(line) {
                let mut k = i + 1;
                while k < n && is_blank(lines[k]) {
                    k += 1;
                }
                if k < n && leading_spaces(lines[k]) == base_indent {
                    let t = lines[k].trim_start();
                    if let Some((m, _)) = list_item(t) {
                        if marker_kind(&m) == kind {
                            loose = true;
                            i = k;
                            continue;
                        }
                    }
                }
                break;
            }
            let t = line.trim_start();
            let ind = leading_spaces(line);
            if ind == base_indent {
                if let Some((m, body)) = list_item(t) {
                    if marker_kind(&m) != kind {
                        break;
                    }
                    let marker_len = list_marker_text(t).chars().count();
                    let after = &t[marker_len..];
                    let spaces = after.chars().take_while(|&c| c == ' ').count().min(4);
                    // marker_len already includes the marker's trailing
                    // space, so extra spaces are only the ones beyond it.
                    let content_indent = if body.is_empty() {
                        ind + marker_len + 1
                    } else {
                        ind + marker_len + spaces
                    };
                    let (marker_out, body_out) = match task_checkbox(body) {
                        Some((cb, rest)) => (cb, rest.to_string()),
                        None => (list_marker_text(t), body.to_string()),
                    };
                    items.push((marker_out, content_indent, vec![body_out]));
                    i += 1;
                    while i < n {
                        let cl = lines[i];
                        if is_blank(cl) {
                            let mut k = i + 1;
                            while k < n && is_blank(lines[k]) {
                                k += 1;
                            }
                            if k < n && leading_spaces(lines[k]) >= content_indent {
                                loose = true;
                                if let Some(last) = items.last_mut() {
                                    last.2.push(String::new());
                                }
                                i = k;
                                continue;
                            }
                            break;
                        }
                        if leading_spaces(cl) >= content_indent {
                            let stripped = &cl[content_indent.min(cl.len())..];
                            if let Some(last) = items.last_mut() {
                                last.2.push(stripped.to_string());
                            }
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    continue;
                }
            }
            break;
        }
        // Render each item recursively, then re-attach markers.
        let mut out: Vec<Line<'static>> = Vec::new();
        let item_count = items.len();
        for (idx, (marker, content_indent, content)) in items.into_iter().enumerate() {
            let refs: Vec<&str> = content.iter().map(|s| s.as_str()).collect();
            let item_width = self.width.map(|w| w.saturating_sub(content_indent));
            let mut sub = Renderer {
                th: self.th,
                defs: self.defs,
                width: item_width,
                last_was_paragraph: false,
            };
            let mut rendered = sub.render_blocks(&refs);
            while rendered.last().is_some_and(|l| l.width() == 0) {
                rendered.pop();
            }
            if rendered.is_empty() {
                out.push(Line::from(Span::styled(
                    format!("{}{}", " ".repeat(base_indent), marker),
                    Style::default().fg(self.th.dim),
                )));
            } else {
                // First line: marker + first content line.
                let first = rendered.remove(0);
                let mut spans = vec![Span::styled(
                    format!("{}{}", " ".repeat(base_indent), marker),
                    Style::default().fg(self.th.dim),
                )];
                spans.extend(first.spans);
                out.push(Line::from(spans));
                for l in rendered {
                    if l.width() == 0 {
                        out.push(Line::default());
                    } else {
                        let mut spans = vec![Span::styled(
                            " ".repeat(content_indent),
                            Style::default(),
                        )];
                        spans.extend(l.spans);
                        out.push(Line::from(spans));
                    }
                }
            }
            if loose && idx + 1 < item_count {
                out.push(Line::default());
            }
        }
        (out, i)
    }
}


// ---------------------------------------------------------------------------
// Block helpers
// ---------------------------------------------------------------------------

/// Approximate display width of one char: 2 for wide CJK ranges, 1
/// otherwise (0 for control chars). Exact Unicode width would need
/// the unicode-width crate; prose wrapping only needs to be close,
/// and no text is ever dropped by an off-by-one here.
fn char_width(c: char) -> usize {
    if c.is_control() {
        0
    } else {
        match c as u32 {
            0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6 => 2,
            _ => 1,
        }
    }
}

/// Word-wrap one styled line to `width`, breaking at the last space
/// that fits, or mid-word when a single word is longer than the
/// width. Styled spans survive: chars are grouped back by style.
/// Continuation lines are prefixed with `cont_indent` spaces.
/// No input char is ever dropped: spaces used as break points are
/// the only chars removed, and only at a line boundary.
fn wrap_line(line: Line<'static>, width: usize, cont_indent: usize) -> Vec<Line<'static>> {
    let cont_indent = cont_indent.min(width.saturating_sub(1));
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|sp| sp.content.chars().map(move |c| (c, sp.style)))
        .collect();
    let flush = |cur: &[(char, Style)]| -> Line<'static> {
        // Group consecutive same-style chars back into spans, and
        // trim trailing spaces so wrapped lines do not end in blanks.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut buf = String::new();
        let mut cur_style: Option<Style> = None;
        for &(c, style) in cur {
            if cur_style != Some(style) {
                if !buf.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut buf),
                        cur_style.unwrap(),
                    ));
                }
                cur_style = Some(style);
            }
            buf.push(c);
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, cur_style.unwrap()));
        }
        // Trim trailing spaces from the last span.
        if let Some(last) = spans.last_mut() {
            let trimmed = last.content.trim_end_matches(' ').to_string();
            last.content = trimmed.into();
            if last.content.is_empty() {
                spans.pop();
            }
        }
        Line::from(spans)
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    let mut last_space: Option<usize> = None; // index into `cur`
    let mut first = true;

    let start_line = |cur: &mut Vec<(char, Style)>, cur_w: &mut usize, first: &mut bool| {
        cur.clear();
        if !*first {
            for _ in 0..cont_indent {
                cur.push((' ', Style::default()));
            }
        }
        *cur_w = if *first { 0 } else { cont_indent };
        *first = false;
    };
    start_line(&mut cur, &mut cur_w, &mut first);

    for &(c, style) in &chars {
        let cw = char_width(c);
        if cur_w + cw > width && !cur.iter().all(|&(ch, _)| ch == ' ') {
            // Break before this char.
            if let Some(sp) = last_space.filter(|&sp| sp > cont_indent || first) {
                // Break at the last space: flush up to it, carry the
                // remainder (after the space) to the next line.
                let before = cur[..sp].to_vec();
                let mut remainder = cur[sp + 1..].to_vec();
                // The remainder may itself contain spaces; the last
                // one is the new break candidate.
                let mut new_last: Option<usize> = None;
                for (idx, &(ch, _)) in remainder.iter().enumerate() {
                    if ch == ' ' {
                        new_last = Some(idx + cont_indent);
                    }
                }
                out.push(flush(&before));
                start_line(&mut cur, &mut cur_w, &mut first);
                cur.append(&mut remainder);
                cur_w += remainder_len(&cur, cont_indent);
                last_space = new_last;
                // Now place the current char (fall through to append).
            } else {
                out.push(flush(&cur));
                start_line(&mut cur, &mut cur_w, &mut first);
                last_space = None;
            }
        }
        if c == ' ' {
            last_space = Some(cur.len());
        }
        cur.push((c, style));
        cur_w += cw;
    }
    if !cur.iter().all(|&(ch, _)| ch == ' ') {
        out.push(flush(&cur));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// Width of `cur` minus its continuation prefix, for recomputing
/// `cur_w` after a break carries a remainder onto a new line.
fn remainder_len(cur: &[(char, Style)], cont_indent: usize) -> usize {
    cur.iter()
        .skip(cont_indent)
        .map(|&(c, _)| char_width(c))
        .sum()
}


fn is_blank(s: &str) -> bool {
    s.trim().is_empty()
}

fn leading_spaces(s: &str) -> usize {
    s.chars().take_while(|&c| c == ' ').count()
}

/// `# Title` → `Some("Title")`; 1-6 hashes followed by a space, or
/// hashes alone (empty heading). A closing hash sequence
/// (`## Title ##`) is stripped. More than 6 hashes is not a heading.
fn atx_heading(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if rest.is_empty() {
        return Some("");
    }
    if !rest.starts_with(' ') {
        return None;
    }
    let mut text = rest.trim();
    let stripped = text.trim_end_matches('#');
    if stripped != text && stripped.ends_with(' ') {
        text = stripped.trim_end();
    }
    Some(text)
}

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
    let mut digits = 0;
    for c in trimmed.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    !(digits > 0 && (trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") ")))
}

fn is_hr(trimmed: &str) -> bool {
    let chars: Vec<char> = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    chars.len() >= 3
        && chars.iter().all(|&c| c == chars[0])
        && (chars[0] == '-' || chars[0] == '*' || chars[0] == '_')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
    Bullet(char),
    Ordered(u32),
}

fn list_marker_text(trimmed: &str) -> String {
    for marker in ["- ", "* ", "+ "] {
        if trimmed.starts_with(marker) {
            return marker.to_string();
        }
    }
    let mut digits = 0;
    for c in trimmed.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    if digits > 0 && digits <= 9 {
        if trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") ") {
            return trimmed[..digits + 2].to_string();
        }
        if &trimmed[digits..] == "." || &trimmed[digits..] == ")" {
            return trimmed[..digits + 1].to_string();
        }
    }
    String::new()
}

fn marker_kind(marker: &str) -> MarkerKind {
    if let Some(c) = marker.chars().next() {
        if c == '-' || c == '*' || c == '+' {
            return MarkerKind::Bullet(c);
        }
    }
    let num: String = marker.chars().take_while(|c| c.is_ascii_digit()).collect();
    MarkerKind::Ordered(num.parse().unwrap_or(0))
}

fn marker_is_empty_item(trimmed: &str, marker: &str) -> bool {
    trimmed == marker.trim_end() || trimmed[marker.len()..].trim().is_empty()
}

/// `- item` / `* item` / `+ item` (long only; short `+ ` lines are tags) /
/// `1. item` / `1) item` → `Some((marker, body))`. Ordered markers keep
/// their number; a trailing marker with no body is an empty item.
fn list_item(trimmed: &str) -> Option<(String, &str)> {
    for marker in ["- ", "* "] {
        if let Some(body) = trimmed.strip_prefix(marker) {
            return Some((marker.to_string(), body));
        }
        if trimmed == marker.trim_end() {
            return Some((marker.to_string(), ""));
        }
    }
    if trimmed.chars().count() > SHORT_LINE {
        if let Some(body) = trimmed.strip_prefix("+ ") {
            return Some(("+ ".to_string(), body));
        }
        if trimmed == "+" {
            return Some(("+ ".to_string(), ""));
        }
    }
    let mut digits = 0;
    for c in trimmed.chars() {
        if c.is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    if digits > 0 && digits <= 9 {
        if trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") ") {
            let marker_len = digits + 2;
            return Some((trimmed[..marker_len].to_string(), &trimmed[marker_len..]));
        }
        if &trimmed[digits..] == "." || &trimmed[digits..] == ")" {
            return Some((trimmed[..digits + 1].to_string(), ""));
        }
    }
    None
}

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

fn is_section_header(lines: &[&str], i: usize) -> bool {
    let trimmed = lines[i].trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > SHORT_LINE
        || trimmed.ends_with(':')
        // A short ordinary sentence is not a heading: headings do
        // not end in sentence punctuation. Without this, any short
        // sentence after a blank line rendered as a heading.
        || trimmed.ends_with('.')
        || trimmed.ends_with('!')
        || trimmed.ends_with('?')
    {
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
    if digits > 0 && (trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") ")) {
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

// --- fences ---

/// Fence opening: (char, length, info) or None. Backtick info strings
/// may not contain backticks; tilde info strings may contain anything
/// except a newline.
fn fence_open(trimmed: &str) -> Option<(char, usize, String)> {
    let c = trimmed.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|&ch| ch == c).count();
    if len < 3 {
        return None;
    }
    let info = trimmed[len..].trim().to_string();
    if c == '`' && info.contains('`') {
        return None;
    }
    Some((c, len, info))
}

fn fence_close(trimmed: &str, fence_char: char) -> Option<(char, usize)> {
    let c = trimmed.chars().next()?;
    if c != fence_char {
        return None;
    }
    let len = trimmed.chars().take_while(|&ch| ch == fence_char).count();
    if len < 3 {
        return None;
    }
    let rest = trimmed[len..].trim();
    if rest.is_empty() {
        Some((c, len))
    } else {
        None
    }
}

// --- HTML blocks ---

fn html_tag_name(trimmed: &str) -> Option<(String, bool)> {
    // Returns (tag name lowercased, is_closing) for a line starting
    // with a tag, or None.
    let s = trimmed.strip_prefix('<')?;
    let s = s.strip_prefix('/').map(|r| (r, true)).unwrap_or((s, false));
    let (s, closing) = s;
    let mut name = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            name.push(c.to_ascii_lowercase());
        } else {
            break;
        }
    }
    if name.is_empty() {
        None
    } else {
        Some((name, closing))
    }
}

/// HTML block end index (exclusive) starting at `i`, or None if the
/// line does not start an HTML block. Types 1-6 run to a blank line;
/// type 7 (a complete tag alone on a line) is a single line.
fn html_block_end(lines: &[&str], i: usize, last_was_paragraph: bool) -> Option<usize> {
    let trimmed = lines[i].trim_start();
    if !trimmed.starts_with('<') {
        return None;
    }
    // Comment, processing instruction, declaration, CDATA: to the
    // closing marker (GFM 4.6 types 2-5), like script/pre/style/
    // textarea run to their end tag. Running to a blank line instead
    // swallowed the first paragraph after a multi-line comment.
    if trimmed.starts_with("<!--") || trimmed.starts_with("<?") || trimmed.starts_with("<!") {
        let marker = if trimmed.starts_with("<!--") {
            "-->"
        } else if trimmed.starts_with("<?") {
            "?>"
        } else if trimmed.starts_with("<![CDATA[") {
            "]]>"
        } else {
            // Declaration: ends at the first '>' (usually on this line).
            ">"
        };
        // The opening line itself may already contain the marker
        // (e.g. a one-line comment); scan from `i`, not `i + 1`.
        let mut j = i;
        while j < lines.len() {
            if lines[j].contains(marker) {
                return Some(j + 1);
            }
            j += 1;
        }
        return Some(j);
    }
    let Some((name, _closing)) = html_tag_name(trimmed) else {
        return None;
    };
    if !BLOCK_TAGS.contains(&name.as_str()) && !DISALLOWED_TAGS.contains(&name.as_str()) {
        // Type 7 candidate: a complete tag alone on a line, any tag
        // name. It may not interrupt a paragraph (GFM 4.6).
        if last_was_paragraph {
            return None;
        }
        // Complete tag alone on the line?
        if trimmed.ends_with('>') {
            if let Some(end) = scan_html_tag(trimmed, 0) {
                if end == trimmed.len() {
                    let mut j = i + 1;
                    while j < lines.len() && !is_blank(lines[j]) {
                        j += 1;
                    }
                    return Some(j);
                }
            }
        }
        return None;
    }
    // Type 1 (script/pre/style/textarea): run to the matching end
    // tag, blank lines included (GFM 4.6).
    if matches!(name.as_str(), "script" | "pre" | "style" | "textarea") {
        let end_tag = format!("</{name}");
        let mut j = i + 1;
        while j < lines.len() {
            if lines[j].to_lowercase().contains(&end_tag) {
                return Some(j + 1);
            }
            j += 1;
        }
        return Some(j);
    }
    // Type 7: the whole line is just the tag (open or close), possibly
    // with attributes, and nothing else. Its end condition is the
    // same as types 1/6: a blank line (GFM 4.6) — it is NOT a single
    // line, so following lines stay literal until a blank line.
    // (Type 7 may not interrupt a paragraph; that is enforced by the
    // caller passing last_was_paragraph — see render_blocks.)
    let _ = trimmed;
    // Types 1/6/7: to blank line.
    let mut j = i + 1;
    while j < lines.len() && !is_blank(lines[j]) {
        j += 1;
    }
    Some(j)
}

// --- block quotes ---

fn is_quote_start(trimmed: &str) -> bool {
    trimmed.starts_with('>') && (trimmed.len() == 1 || trimmed[1..].starts_with(' '))
}

fn strip_quote_marker(line: &str) -> String {
    let t = line.trim_start();
    let after = &t[1..];
    if let Some(rest) = after.strip_prefix(' ') {
        rest.to_string()
    } else {
        after.to_string()
    }
}

/// Whether `text` ends with a backtick run the inline parser would
/// leave unclosed. This mirrors `parse_inline_nodes` exactly: an
/// escaped backtick never opens a run, but the closing search is raw,
/// so an escaped backtick can close one; a run closes only on a later
/// run of exactly the same length, and content inside a closed span
/// cannot open another.
fn unclosed_backtick_run(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && chars[i + 1].is_ascii_punctuation() {
            i += 2;
            continue;
        }
        if c == '`' {
            let mut j = i;
            while j < chars.len() && chars[j] == '`' {
                j += 1;
            }
            let run = j - i;
            let mut k = j;
            let mut close: Option<usize> = None;
            while k < chars.len() {
                if chars[k] == '`' {
                    let mut m = k;
                    while m < chars.len() && chars[m] == '`' {
                        m += 1;
                    }
                    if m - k == run {
                        close = Some(k);
                        break;
                    }
                    k = m;
                } else {
                    k += 1;
                }
            }
            match close {
                Some(at) => i = at + run,
                None => return true,
            }
            continue;
        }
        i += 1;
    }
    false
}

/// Whether the block loop, arriving at `lines[j]` with a paragraph
/// open, would render it as a plain body line rather than opening a
/// new block. Only such lines may continue a joined code span:
/// swallowing a heading, fence, list item, or other block start
/// would eat structure that belongs to the next block. Indented
/// lines count (indented code cannot interrupt a paragraph), and
/// the list check is deliberately blanket — a non-interrupting list
/// line simply ends the join instead of risking a wrong one.
fn is_plain_body_continuation(lines: &[&str], j: usize) -> bool {
    let line = lines[j];
    let trimmed = line.trim();
    if is_blank(line) {
        return false;
    }
    if leading_spaces(line) > 3 {
        return true;
    }
    if parse_def_at(lines, j).is_some()
        || fence_open(trimmed).is_some()
        || html_block_end(lines, j, true).is_some()
        || is_table_start(lines, j)
        || atx_heading(trimmed).is_some()
        || is_hr(trimmed)
        || is_quote_start(trimmed)
        || list_item(trimmed).is_some()
        || is_section_header(lines, j)
    {
        return false;
    }
    // A setext underline shape, or a short `+ ...` tag line: both
    // render as something other than body text when reached fresh.
    if trimmed.chars().all(|c| c == '=' || c == '-') {
        return false;
    }
    if trimmed.chars().count() <= SHORT_LINE && trimmed.starts_with("+ ") {
        return false;
    }
    true
}

/// Collect a blockquote starting at `i`: quote lines (markers
/// stripped) plus lazy continuation lines (paragraph lines without a
/// marker that follow a quote paragraph and are not a new block).
/// Returns (inner lines, next index).
fn collect_quote(lines: &[&str], start: usize) -> (Vec<String>, usize) {
    let mut inner: Vec<String> = Vec::new();
    let mut i = start;
    let n = lines.len();
    let mut in_paragraph = false;
    while i < n {
        let line = lines[i];
        if is_blank(line) {
            // Blank line: look ahead — if a quote line follows, the
            // blank belongs to the quote (as an empty inner line).
            let mut k = i + 1;
            while k < n && is_blank(lines[k]) {
                k += 1;
            }
            if k < n && is_quote_start(lines[k].trim_start()) {
                inner.push(String::new());
                i = k;
                in_paragraph = false;
                continue;
            }
            break;
        }
        let trimmed = line.trim_start();
        if is_quote_start(trimmed) {
            inner.push(strip_quote_marker(line));
            in_paragraph = true;
            i += 1;
            continue;
        }
        // Lazy continuation: only while in a paragraph, and only for
        // lines that are not a new block starter.
        if in_paragraph
            && leading_spaces(line) <= 3
            && !is_hr(trimmed)
            && atx_heading(trimmed).is_none()
            && list_item(trimmed).is_none()
            && fence_open(trimmed).is_none()
            && !is_table_start(lines, i)
        {
            inner.push(line.to_string());
            i += 1;
            continue;
        }
        break;
    }
    (inner, i)
}

// --- link reference definitions ---

fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Parse a single-line link reference definition. Returns
/// (label, destination) or None. Titles are parsed (and validated)
/// but not stored, since the terminal drops them.
fn parse_def_line(trimmed: &str) -> Option<(String, String)> {
    let rest = trimmed.strip_prefix('[')?;
    // Label: up to the first unescaped ']'.
    let mut label = String::new();
    let mut chars = rest.char_indices().peekable();
    let mut close_idx: Option<usize> = None;
    while let Some((idx, c)) = chars.next() {
        if c == '\\' {
            if let Some((_, nc)) = chars.next() {
                label.push(nc);
            }
            continue;
        }
        if c == ']' {
            close_idx = Some(idx);
            break;
        }
        if c == '[' {
            return None;
        }
        label.push(c);
    }
    let close_idx = close_idx?;
    if label.trim().is_empty() || label.chars().count() > 999 {
        return None;
    }
    let after = &rest[close_idx + 1..];
    let after = after.strip_prefix(':')?;
    let after = after.trim_start();
    if after.is_empty() {
        return None;
    }
    // Destination: <...> or bare.
    let (dest, mut rest2) = if let Some(a) = after.strip_prefix('<') {
        let end = a.find('>')?;
        let d = &a[..end];
        if d.contains('<') || d.contains('\n') {
            return None;
        }
        (d.to_string(), &a[end + 1..])
    } else {
        // Bare destination: up to whitespace, balanced parens.
        let mut depth = 0i32;
        let mut end = 0usize;
        for (idx, c) in after.char_indices() {
            if c.is_whitespace() {
                break;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            end = idx + c.len_utf8();
        }
        if end == 0 {
            return None;
        }
        (after[..end].to_string(), &after[end..])
    };
    rest2 = rest2.trim_start();
    if rest2.is_empty() {
        return Some((normalize_label(&label), dest));
    }
    // Optional title: "..." , '...' , or (...), then only spaces.
    let title_end = parse_title(rest2)?;
    if rest2[title_end..].trim().is_empty() {
        Some((normalize_label(&label), dest))
    } else {
        None
    }
}

/// Parse a link reference definition starting at `lines[i]`, which
/// may span lines: the destination and/or the title may sit on the
/// following line(s) (GFM 4.7: a definition is not confined to one
/// line). Returns (label, destination, lines consumed), or None.
/// A blank line ends the attempt; at most 3 lines are consumed.
fn parse_def_at(lines: &[&str], i: usize) -> Option<(String, String, usize)> {
    let first = lines[i].trim_start();
    if leading_spaces(lines[i]) > 3 {
        return None;
    }
    if let Some((label, dest)) = parse_def_line(first) {
        // Single-line def, but a title alone on the next line still
        // belongs to it (e.g. `[foo]: /url` followed by `"Title"`):
        // consume it only when line 1 carried no title of its own.
        let mut consumed = 1;
        if first_def_remainder(first).is_empty() && i + 1 < lines.len() && !is_blank(lines[i + 1]) {
            let next = lines[i + 1].trim();
            if let Some(end) = parse_title(next) {
                if next[end..].trim().is_empty() {
                    consumed = 2;
                }
            }
        }
        return Some((label, dest, consumed));
    }
    // Multi-line: label + colon with the destination (or a title
    // completing a partial one) on the next line(s).
    let mut joined = first.to_string();
    for extra in 1..=2 {
        let Some(next) = lines.get(i + extra) else {
            break;
        };
        if is_blank(next) {
            break;
        }
        joined.push(' ');
        joined.push_str(next.trim());
        if let Some((label, dest)) = parse_def_line(&joined) {
            // As in the single-line case, a title alone on the next
            // line still belongs to this definition.
            let mut consumed = extra + 1;
            if first_def_remainder(&joined).is_empty() {
                if let Some(title_line) = lines.get(i + consumed) {
                    if !is_blank(title_line) {
                        let t = title_line.trim();
                        if let Some(end) = parse_title(t) {
                            if t[end..].trim().is_empty() {
                                consumed += 1;
                            }
                        }
                    }
                }
            }
            return Some((label, dest, consumed));
        }
    }
    None
}

/// Remainder of a single-line def after its destination, trimmed:
/// empty when the line carries no title. Used to decide whether a
/// title on the following line belongs to this definition.
fn first_def_remainder(trimmed: &str) -> String {
    let Some(rest) = trimmed.strip_prefix('[') else {
        return trimmed.to_string();
    };
    let Some(close) = rest.find(']') else {
        return trimmed.to_string();
    };
    let Some(after) = rest[close + 1..].strip_prefix(':') else {
        return trimmed.to_string();
    };
    let after = after.trim_start();
    if let Some(a) = after.strip_prefix('<') {
        if let Some(end) = a.find('>') {
            return a[end + 1..].trim().to_string();
        }
        return after.to_string();
    }
    let mut depth = 0i32;
    let mut end = 0usize;
    for (idx, c) in after.char_indices() {
        if c.is_whitespace() {
            break;
        }
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            if depth == 0 {
                break;
            }
            depth -= 1;
        }
        end = idx + c.len_utf8();
    }
    after[end..].trim().to_string()
}

/// Parse a title at the start of `s`; returns the byte index just
/// past the title, or None.
fn parse_title(s: &str) -> Option<usize> {
    let first = s.chars().next()?;
    match first {
        '"' | '\'' => {
            let mut idx = 1usize;
            for c in s[1..].chars() {
                if c == '\\' {
                    idx += 1;
                    // Skip the escaped char below via char iteration
                    // bookkeeping: we advance by its length separately.
                    // Simpler: iterate by byte index in the caller path
                    // — handled by the loop below (see parse_title_bytes).
                    let _ = idx;
                    break;
                }
                if c == first {
                    return Some(idx + c.len_utf8());
                }
                idx += c.len_utf8();
            }
            // Fall through to the byte-accurate parser.
            parse_title_bytes(s)
        }
        '(' => {
            let mut depth = 0i32;
            for (idx, c) in s.char_indices() {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(idx + 1);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn parse_title_bytes(s: &str) -> Option<usize> {
    let first = s.as_bytes()[0];
    let mut i = 1usize;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == first {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn collect_defs(lines: &[&str]) -> Defs {
    let mut defs: Defs = HashMap::new();
    let mut in_fence: Option<(char, usize)> = None;
    let mut in_indented = false;
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let indent = leading_spaces(line);
        if let Some((fc, flen)) = in_fence {
            if indent <= 3 {
                if let Some((c2, len2)) = fence_close(trimmed, fc) {
                    if len2 >= flen {
                        in_fence = None;
                    }
                }
            }
            i += 1;
            continue;
        }
        if in_indented {
            if is_blank(line) {
                // A blank line belongs to the block only if an
                // indented line follows it.
                let mut k = i + 1;
                while k < lines.len() && is_blank(lines[k]) {
                    k += 1;
                }
                if k < lines.len() && leading_spaces(lines[k]) >= 4 {
                    i += 1;
                    continue;
                }
                in_indented = false;
                i += 1;
                continue;
            }
            if indent >= 4 {
                i += 1;
                continue;
            }
            in_indented = false;
            // Fall through: this line ends the code block.
        }
        if indent >= 4 {
            in_indented = true;
            i += 1;
            continue;
        }
        if indent <= 3 {
            if let Some((fc, flen, _)) = fence_open(trimmed) {
                in_fence = Some((fc, flen));
                i += 1;
                continue;
            }
            // A def-looking line inside an HTML block is literal
            // block content, not a definition: skip the whole block.
            if let Some(end) = html_block_end(lines, i, false) {
                i = end.max(i + 1);
                continue;
            }
            if let Some((label, dest, consumed)) = parse_def_at(lines, i) {
                // First definition wins (GFM 4.7).
                defs.entry(label).or_insert(dest);
                i += consumed;
                continue;
            }
        }
        i += 1;
    }
    defs
}

// --- tables ---

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
    !inner.is_empty()
        && inner.split('|').all(|cell| {
            let c = cell.trim();
            !c.is_empty() && c.chars().all(|ch| ch == ':' || ch == '-') && c.contains('-')
        })
}

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

impl<'a> Renderer<'a> {
    /// Render a pipe table starting at `i`; returns lines and the
    /// index of the first line after the table. Cells are inline-
    /// formatted; the header row is heading-bold, body rows keep
    /// their inline styles.
    fn render_table(&self, lines: &[&str], i: usize) -> (Vec<Line<'static>>, usize) {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut j = i;
        while j < lines.len() && lines[j].contains('|') {
            rows.push(table_cells(lines[j]));
            j += 1;
        }
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
        // Widths are computed from plain cell text (markers stripped),
        // so alignment stays correct with inline formatting.
        let mut widths = vec![0usize; cols];
        for row in &body_rows {
            for (c, cell) in row.iter().enumerate() {
                let plain: String = inline(cell, self.th, self.defs)
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect();
                widths[c] = widths[c].max(plain.chars().count());
            }
        }
        let col_align: Vec<Align> = (0..cols)
            .map(|c| rule_aligns.get(c).copied().unwrap_or(Align::Left))
            .collect();
        let mut out = Vec::new();
        for (ri, row) in body_rows.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for c in 0..cols {
                if c > 0 {
                    spans.push(Span::styled("  ", Style::default()));
                }
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                let mut cell_spans = inline(cell, self.th, self.defs);
                if ri == 0 {
                    for sp in cell_spans.iter_mut() {
                        sp.style.fg = Some(self.th.heading);
                        sp.style = sp.style.add_modifier(Modifier::BOLD);
                    }
                }
                let cell_w: usize = cell_spans
                    .iter()
                    .map(|s| s.content.chars().count())
                    .sum();
                let pad = widths[c].saturating_sub(cell_w);
                match col_align[c] {
                    Align::Left => {
                        spans.extend(cell_spans);
                        if pad > 0 {
                            spans.push(Span::styled(" ".repeat(pad), Style::default()));
                        }
                    }
                    Align::Right => {
                        if pad > 0 {
                            spans.push(Span::styled(" ".repeat(pad), Style::default()));
                        }
                        spans.extend(cell_spans);
                    }
                    Align::Center => {
                        let left = pad / 2;
                        let right = pad - left;
                        if left > 0 {
                            spans.push(Span::styled(" ".repeat(left), Style::default()));
                        }
                        spans.extend(cell_spans);
                        if right > 0 {
                            spans.push(Span::styled(" ".repeat(right), Style::default()));
                        }
                    }
                }
            }
            out.push(Line::from(spans));
        }
        (out, j)
    }
}

// ---------------------------------------------------------------------------
// Inline parsing
// ---------------------------------------------------------------------------

/// Inline node tree. Delimiters are resolved into Em/Strong/Strike
/// nodes by `process_emphasis`; everything else is already final.
#[derive(Debug, Clone)]
enum INode {
    /// Placeholder left where a node was taken into a wrapped node.
    /// Flattens to nothing; avoids Vec shifts during emphasis.
    Empty,
    Text(String),
    Code(String),
    Link(Vec<INode>),
    Image(String),
    Em(Vec<INode>),
    Strong(Vec<INode>),
    Strike(Vec<INode>),
    Delim {
        ch: char,
        len: usize,
        can_open: bool,
        can_close: bool,
    },
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace() && !c.is_control())
}

fn flanking(prev: Option<char>, next: Option<char>) -> (bool, bool) {
    // Left-flanking: not followed by whitespace, and (not followed by
    // punct, or followed by punct and preceded by whitespace/punct).
    let next_ws = next.map(|c| c.is_whitespace()).unwrap_or(true);
    let next_punct = next.map(is_punct).unwrap_or(false);
    let prev_ws = prev.map(|c| c.is_whitespace()).unwrap_or(true);
    let prev_punct = prev.map(is_punct).unwrap_or(false);
    let left = !next_ws && (!next_punct || prev_ws || prev_punct);
    let right = !prev_ws && (!prev_punct || next_ws || next_punct);
    (left, right)
}

/// Decode an entity reference starting at `&` in `s` (byte index 0).
/// Returns (decoded char, bytes consumed) or None.
fn decode_entity(s: &str) -> Option<(char, usize)> {
    if !s.starts_with('&') {
        return None;
    }
    let semi = s.find(';')?;
    if semi == 0 || semi > 32 {
        return None;
    }
    let body = &s[1..semi];
    if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
        let n = u32::from_str_radix(hex, 16).ok()?;
        let c = char::from_u32(n)?;
        if n == 0 {
            return None;
        }
        return Some((c, semi + 1));
    }
    if let Some(dec) = body.strip_prefix('#') {
        if dec.is_empty() || !dec.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let n: u32 = dec.parse().ok()?;
        let c = char::from_u32(n)?;
        if n == 0 {
            return None;
        }
        return Some((c, semi + 1));
    }
    let c = named_entity(body)?;
    Some((c, semi + 1))
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00A0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "laquo" => '«',
        "raquo" => '»',
        "ldquo" => '“',
        "rdquo" => '”',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "bdquo" => '„',
        "dagger" => '†',
        "Dagger" => '‡',
        "permil" => '‰',
        "lsaquo" => '‹',
        "rsaquo" => '›',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "curren" => '¤',
        "sect" => '§',
        "uml" => '¨',
        "ordf" => 'ª',
        "ordm" => 'º',
        "para" => '¶',
        "middot" => '·',
        "cedil" => '¸',
        "sup1" => '¹',
        "sup2" => '²',
        "sup3" => '³',
        "acute" => '´',
        "micro" => 'µ',
        "frac14" => '¼',
        "frac12" => '½',
        "frac34" => '¾',
        "iexcl" => '¡',
        "iquest" => '¿',
        "szlig" => 'ß',
        "agrave" => 'à',
        "aacute" => 'á',
        "acirc" => 'â',
        "atilde" => 'ã',
        "auml" => 'ä',
        "aring" => 'å',
        "aelig" => 'æ',
        "ccedil" => 'ç',
        "egrave" => 'è',
        "eacute" => 'é',
        "ecirc" => 'ê',
        "euml" => 'ë',
        "igrave" => 'ì',
        "iacute" => 'í',
        "icirc" => 'î',
        "iuml" => 'ï',
        "ntilde" => 'ñ',
        "ograve" => 'ò',
        "oacute" => 'ó',
        "ocirc" => 'ô',
        "otilde" => 'õ',
        "ouml" => 'ö',
        "oslash" => 'ø',
        "ugrave" => 'ù',
        "uacute" => 'ú',
        "ucirc" => 'û',
        "uuml" => 'ü',
        "yacute" => 'ý',
        "yuml" => 'ÿ',
        "Agrave" => 'À',
        "Aacute" => 'Á',
        "Acirc" => 'Â',
        "Atilde" => 'Ã',
        "Auml" => 'Ä',
        "Aring" => 'Å',
        "AElig" => 'Æ',
        "Ccedil" => 'Ç',
        "Egrave" => 'È',
        "Eacute" => 'É',
        "Ecirc" => 'Ê',
        "Euml" => 'Ë',
        "Igrave" => 'Ì',
        "Iacute" => 'Í',
        "Icirc" => 'Î',
        "Iuml" => 'Ï',
        "Ntilde" => 'Ñ',
        "Ograve" => 'Ò',
        "Oacute" => 'Ó',
        "Ocirc" => 'Ô',
        "Otilde" => 'Õ',
        "Ouml" => 'Ö',
        "Oslash" => 'Ø',
        "Ugrave" => 'Ù',
        "Uacute" => 'Ú',
        "Ucirc" => 'Û',
        "Uuml" => 'Ü',
        "Yacute" => 'Ý',
        "THORN" => 'Þ',
        "thorn" => 'þ',
        "eth" => 'ð',
        "ETH" => 'Ð',
        "alpha" => 'α',
        "beta" => 'β',
        "gamma" => 'γ',
        "delta" => 'δ',
        "epsilon" => 'ε',
        "pi" => 'π',
        "sigma" => 'σ',
        "omega" => 'ω',
        "Alpha" => 'Α',
        "Beta" => 'Β',
        "Gamma" => 'Γ',
        "Delta" => 'Δ',
        "Pi" => 'Π',
        "Sigma" => 'Σ',
        "Omega" => 'Ω',
        "infin" => '∞',
        "ne" => '≠',
        "le" => '≤',
        "ge" => '≥',
        "plusmn" => '±',
        "times" => '×',
        "divide" => '÷',
        "minus" => '−',
        "lowast" => '∗',
        "radic" => '√',
        "prop" => '∝',
        "empty" => '∅',
        "isin" => '∈',
        "notin" => '∉',
        "sum" => '∑',
        "prod" => '∏',
        "part" => '∂',
        "int" => '∫',
        "forall" => '∀',
        "exist" => '∃',
        "nabla" => '∇',
        "larr" => '←',
        "uarr" => '↑',
        "rarr" => '→',
        "darr" => '↓',
        "harr" => '↔',
        "lArr" => '⇐',
        "uArr" => '⇑',
        "rArr" => '⇒',
        "dArr" => '⇓',
        "hArr" => '⇔',
        "spades" => '♠',
        "clubs" => '♣',
        "hearts" => '♥',
        "diams" => '♦',
        "bull" => '•',
        "prime" => '′',
        "Prime" => '″',
        "oline" => '‾',
        "frasl" => '⁄',
        "weierp" => '℘',
        "image" => 'ℑ',
        "real" => 'ℜ',
        "alefsym" => 'ℵ',
        "crarr" => '↵',
        "lceil" => '⌈',
        "rceil" => '⌉',
        "lfloor" => '⌊',
        "rfloor" => '⌋',
        "lang" => '⟨',
        "rang" => '⟩',
        "loz" => '◊',
        "deg" => '°',
        _ => return None,
    })
}

/// Find the closing bracket for a link starting at `open` (byte index
/// of '['), accounting for nested brackets, escapes and code spans.
/// Returns the byte index of the matching ']' or None.
fn find_link_close(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 2;
            }
            b'`' => {
                // Skip a code span (run length, matching run).
                let mut j = i;
                while j < bytes.len() && bytes[j] == b'`' {
                    j += 1;
                }
                let run = j - i;
                let mut k = j;
                let mut found = None;
                while k < bytes.len() {
                    if bytes[k] == b'`' {
                        let mut m = k;
                        while m < bytes.len() && bytes[m] == b'`' {
                            m += 1;
                        }
                        if m - k == run {
                            found = Some(m);
                            break;
                        }
                        k = m;
                    } else {
                        k += 1;
                    }
                }
                i = found.unwrap_or(bytes.len());
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

/// Parse an inline link destination/title starting at `(` (byte index).
/// Returns (end index just past ')', dest) or None. Titles are
/// validated but not returned.
fn parse_inline_dest(text: &str, open_paren: usize) -> Option<(usize, String)> {
    let bytes = text.as_bytes();
    let mut i = open_paren + 1;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n') {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    // Empty destination: [text]() is a valid inline link.
    if bytes[i] == b')' {
        return Some((i + 1, String::new()));
    }
    let dest;
    if bytes[i] == b'<' {
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != b'>' && bytes[j] != b'\n' {
            if bytes[j] == b'<' {
                return None;
            }
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'>' {
            return None;
        }
        dest = text[i + 1..j].to_string();
        i = j + 1;
    } else {
        // Bare destination: balanced parens, escapes, no spaces.
        let mut depth = 0i32;
        let start = i;
        let mut end = i;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' && i + 1 < bytes.len() && (bytes[i + 1] as char).is_ascii_punctuation() {
                i += 2;
                end = i;
                continue;
            }
            if c == b' ' || c == b'\t' || c == b'\n' {
                break;
            }
            if c == b'(' {
                depth += 1;
            } else if c == b')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            i += 1;
            end = i;
        }
        if end == start {
            return None;
        }
        dest = text[start..end].to_string();
    }
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n') {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b')' {
        return Some((i + 1, dest));
    }
    // Optional title, then ')'.
    if i < bytes.len() {
        let c = bytes[i] as char;
        if c == '"' || c == '\'' || c == '(' {
            let title = &text[i..];
            let tend = parse_title(title)?;
            i += tend;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b')' {
                return Some((i + 1, dest));
            }
        }
    }
    None
}

/// Try to parse a link or image starting at byte index `start` in
/// `text`. `is_image` means the caller already consumed '!'.
/// Returns (end index, link-text nodes, is_image) or None.
fn parse_link_at(
    text: &str,
    start: usize,
    defs: &Defs,
    th: &Theme,
) -> Option<(usize, Vec<INode>, bool)> {
    let is_image = start > 0 && text.as_bytes()[start - 1] == b'!';
    let open = start;
    let close = find_link_close(text, open)?;
    let link_text = &text[open + 1..close];
    let after = close + 1;
    let bytes = text.as_bytes();
    // Inline link: ](
    if after < bytes.len() && bytes[after] == b'(' {
        if let Some((end, _dest)) = parse_inline_dest(text, after) {
            let nodes = parse_inline_nodes(link_text, th, defs, false);
            return Some((end, nodes, is_image));
        }
        return None;
    }
    // Reference link: ][
    if after < bytes.len() && bytes[after] == b'[' {
        if let Some(close2) = find_link_close(text, after) {
            let label = &text[after + 1..close2];
            let label_norm = if label.is_empty() {
                normalize_label(link_text)
            } else {
                normalize_label(label)
            };
            if defs.contains_key(&label_norm) {
                let nodes = parse_inline_nodes(link_text, th, defs, false);
                return Some((close2 + 1, nodes, is_image));
            }
        }
        return None;
    }
    // Shortcut reference: ] alone, label = link text.
    let label_norm = normalize_label(link_text);
    if defs.contains_key(&label_norm) {
        let nodes = parse_inline_nodes(link_text, th, defs, false);
        return Some((after, nodes, is_image));
    }
    None
}

fn is_email_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '.' | '!' | '#' | '$' | '%' | '&' | '\'' | '*' | '+' | '-' | '/' | '=' | '?' | '^'
                | '_' | '`' | '{' | '|' | '}' | '~'
        )
}

fn valid_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    if local.is_empty()
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || !local.chars().all(is_email_local_char)
    {
        return false;
    }
    if domain.is_empty() {
        return false;
    }
    for label in domain.split('.') {
        if label.is_empty()
            || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            || label.starts_with('-')
            || label.ends_with('-')
        {
            return false;
        }
    }
    true
}

fn valid_domain(s: &str) -> bool {
    if !s.contains('.') {
        return false;
    }
    for label in s.split('.') {
        if label.is_empty()
            || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            || label.starts_with('-')
            || label.ends_with('-')
        {
            return false;
        }
    }
    true
}

/// Autolink in <...>: scheme or email. Returns (content, end index).
fn try_angle_autolink(text: &str, start: usize) -> Option<(String, usize)> {
    let rest = &text[start..];
    let close = rest.find('>')?;
    let content = &rest[1..close];
    if content.is_empty() || content.contains(|c: char| c.is_whitespace() || (c as u32) < 32) {
        return None;
    }
    // Scheme: letter, then letters/digits/+.-, 2-32 chars, then ':'.
    let mut scheme_ok = false;
    if let Some(colon) = content.find(':') {
        let scheme = &content[..colon];
        if (2..=32).contains(&scheme.len())
            && scheme
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-')
        {
            scheme_ok = true;
        }
    }
    if scheme_ok || valid_email(content) {
        Some((content.to_string(), start + close + 1))
    } else {
        None
    }
}

/// Scan an inline HTML tag starting at `start` ('<'). Returns the end
/// index just past '>' if it looks like a tag/comment/PI/declaration/
 /// CDATA, else None. The tag is rendered literally.
fn scan_html_tag(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    if rest.starts_with("<!--") {
        let end = rest.find("-->")?;
        return Some(start + end + 3);
    }
    if rest.starts_with("<?") {
        let end = rest.find("?>")?;
        return Some(start + end + 2);
    }
    if rest.starts_with("<!") {
        // Declaration or CDATA: to the next '>'.
        let end = rest.find('>')?;
        return Some(start + end + 1);
    }
    let bytes = rest.as_bytes();
    let mut i = 1usize;
    if i < bytes.len() && bytes[i] == b'/' {
        i += 1;
    }
    if i >= bytes.len() || !(bytes[i] as char).is_ascii_alphabetic() {
        return None;
    }
    while i < bytes.len() && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'-') {
        i += 1;
    }
    // After the tag name, only whitespace, '/' or '>' may follow;
    // anything else (e.g. '@' in <foo@bar>) is not a tag at all, so
    // the text can still be parsed as an autolink.
    if i < bytes.len() && !(bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'/' || bytes[i] == b'>') {
        return None;
    }
    // Attributes / self-closing / close, respecting quoted strings.
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if b == b'>' {
            return Some(start + i + 1);
        }
        i += 1;
    }
    None
}

/// Trim trailing punctuation from a bare autolink candidate, with
/// balanced-paren handling for ')'. Returns the trimmed length.
fn trim_bare_url(raw: &str) -> usize {
    let mut end = raw.len();
    loop {
        if end == 0 {
            break;
        }
        let last = raw[..end].chars().last().unwrap();
        if last == ';' {
            // A ';' completing an entity reference (e.g. `&amp;` at
            // the end of a bare URL) is part of the URL, not trailing
            // punctuation: keep it instead of stripping it.
            let prefix = &raw[..end - 1];
            if let Some(amp) = prefix.rfind('&') {
                let body = &prefix[amp + 1..];
                if !body.is_empty()
                    && body.len() <= 32
                    && body
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '#')
                {
                    break;
                }
            }
            end -= 1;
            continue;
        }
        if matches!(last, '.' | ',' | ':' | '!' | '?' | '\'' | '"' | ']' | '}') {
            end -= last.len_utf8();
            continue;
        }
        if last == ')' {
            let open = raw[..end].chars().filter(|&c| c == '(').count();
            let close = raw[..end].chars().filter(|&c| c == ')').count();
            if close > open {
                end -= 1;
                continue;
            }
        }
        break;
    }
    end
}

/// Bare autolinks (the GFM extension) need a dotted domain, unlike
/// `<...>` autolinks where `foo@bar` is valid per the spec's email
/// pattern. Requiring the dot keeps `user@host` prose from linking.
fn valid_bare_email(s: &str) -> bool {
    valid_email(s) && s.split_once('@').is_some_and(|(_, d)| d.contains('.'))
}

/// Tokenize `text` into inline nodes (delimiters unresolved).
fn parse_inline_nodes(text: &str, th: &Theme, defs: &Defs, allow_links: bool) -> Vec<INode> {
    let _ = th;
    let chars: Vec<char> = text.chars().collect();
    // Byte offset of each char index, precomputed once: the previous
    // per-char byte_index() scan was O(n^2) and hung on long inputs.
    let byte_offsets: Vec<usize> = {
        let mut v = Vec::with_capacity(chars.len() + 1);
        let mut acc = 0usize;
        for c in &chars {
            v.push(acc);
            acc += c.len_utf8();
        }
        v.push(acc);
        v
    };
    let mut nodes: Vec<INode> = Vec::new();
    let mut buf = String::new();
    let mut flush = |buf: &mut String, nodes: &mut Vec<INode>| {
        if !buf.is_empty() {
            nodes.push(INode::Text(std::mem::take(buf)));
        }
    };
    let mut i = 0usize; // char index
    while i < chars.len() {
        let c = chars[i];
        let bi = byte_offsets[i];
        let rest_str = &text[bi..];
        // Backslash escape.
        if c == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            if next.is_ascii_punctuation() {
                buf.push(next);
                i += 2;
                continue;
            }
            // Backslash before a newline is a hard break (line break
            // already exists in the terminal); otherwise literal.
            buf.push('\\');
            i += 1;
            continue;
        }
        // Code span: backtick run.
        if c == '`' {
            let mut j = i;
            while j < chars.len() && chars[j] == '`' {
                j += 1;
            }
            let run = j - i;
            // Find a closing run of exactly `run` backticks.
            let mut k = j;
            let mut found: Option<usize> = None;
            while k < chars.len() {
                if chars[k] == '`' {
                    let mut m = k;
                    while m < chars.len() && chars[m] == '`' {
                        m += 1;
                    }
                    if m - k == run {
                        found = Some(k);
                        break;
                    }
                    k = m;
                } else {
                    k += 1;
                }
            }
            if let Some(close) = found {
                let mut content: String = chars[j..close].iter().collect();
                // Strip one leading and one trailing space if both
                // present and the content is not all spaces.
                if content.len() >= 2
                    && content.starts_with(' ')
                    && content.ends_with(' ')
                    && !content.trim().is_empty()
                {
                    content = content[1..content.len() - 1].to_string();
                }
                flush(&mut buf, &mut nodes);
                nodes.push(INode::Code(content));
                // Skip the closing run.
                i = close + run;
                continue;
            }
            // No closing run: literal backticks.
            for _ in 0..run {
                buf.push('`');
            }
            i = j;
            continue;
        }
        // Image / link.
        if allow_links && (c == '[' || (c == '!' && i + 1 < chars.len() && chars[i + 1] == '[')) {
            let start = if c == '!' { bi + 1 } else { bi };
            if let Some((end, inner, is_image)) = parse_link_at(text, start, defs, th) {
                flush(&mut buf, &mut nodes);
                if is_image {
                    let alt: String = inner
                        .iter()
                        .flat_map(|n| node_plain_text(n))
                        .collect();
                    nodes.push(INode::Image(alt));
                } else {
                    nodes.push(INode::Link(inner));
                }
                // Advance char index past `end` bytes, counting
                // from the current position (not from the start of
                // the text — counting from zero per link was O(n^2)).
                let mut consumed = i;
                let mut bi2 = bi;
                while bi2 < end && consumed < chars.len() {
                    bi2 += chars[consumed].len_utf8();
                    consumed += 1;
                }
                i = consumed;
                continue;
            }
            // Not a link: fall through to literal handling below.
            if c == '!' {
                buf.push('!');
                i += 1;
                continue;
            }
        }
        // Angle autolink / HTML tag.
        if c == '<' {
            if let Some((content, end)) = try_angle_autolink(text, bi) {
                flush(&mut buf, &mut nodes);
                nodes.push(INode::Link(vec![INode::Text(content)]));
                let mut consumed = i;
                let mut bi2 = bi;
                while bi2 < end && consumed < chars.len() {
                    bi2 += chars[consumed].len_utf8();
                    consumed += 1;
                }
                i = consumed;
                continue;
            }
            if let Some(end) = scan_html_tag(text, bi) {
                // Literal HTML (including disallowed tags).
                let tag: String = chars[i..].iter().collect::<String>();
                let tag_len_bytes = end - bi;
                let tag_text: String = tag.chars().take_while(|_| true).collect();
                let _ = tag_text;
                // Take the tag substring by bytes.
                let full = &text[bi..end];
                buf.push_str(full);
                let mut consumed = i;
                let mut bi2 = bi;
                while bi2 < end && consumed < chars.len() {
                    bi2 += chars[consumed].len_utf8();
                    consumed += 1;
                }
                i = consumed;
                let _ = tag_len_bytes;
                continue;
            }
            buf.push('<');
            i += 1;
            continue;
        }
        // Entity reference.
        if c == '&' {
            if let Some((decoded, consumed)) = decode_entity(rest_str) {
                buf.push(decoded);
                let mut adv = i;
                let mut bi2 = 0usize;
                while bi2 < consumed && adv < chars.len() {
                    bi2 += chars[adv].len_utf8();
                    adv += 1;
                }
                i = adv;
                continue;
            }
            buf.push('&');
            i += 1;
            continue;
        }
        // Emphasis / strikethrough delimiter runs.
        if c == '*' || c == '_' || c == '~' {
            let mut j = i;
            while j < chars.len() && chars[j] == c {
                j += 1;
            }
            let run = j - i;
            let prev = if i == 0 { None } else { Some(chars[i - 1]) };
            let next = if j < chars.len() { Some(chars[j]) } else { None };
            let (left, right) = flanking(prev, next);
            let (can_open, can_close) = match c {
                '*' | '~' => (left, right),
                '_' => (
                    left && (!right || prev.map(is_punct).unwrap_or(false)),
                    right && (!left || next.map(is_punct).unwrap_or(false)),
                ),
                _ => (left, right),
            };
            flush(&mut buf, &mut nodes);
            if c == '~' {
                // Strikethrough uses pairs of tildes; a lone trailing
                // tilde in an odd run is literal text.
                let pairs = run / 2;
                let leftover = run % 2;
                if pairs > 0 {
                    nodes.push(INode::Delim {
                        ch: '~',
                        len: pairs * 2,
                        can_open,
                        can_close,
                    });
                }
                if leftover == 1 {
                    nodes.push(INode::Text("~".to_string()));
                }
            } else {
                nodes.push(INode::Delim {
                    ch: c,
                    len: run,
                    can_open,
                    can_close,
                });
            }
            i = j;
            continue;
        }
        // Bare autolinks (extension): www., bare email, scheme URLs.
        if allow_links {
            // www.
            if rest_str.starts_with("www.") {
                let prev_ok = i == 0
                    || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == '.');
                if prev_ok {
                    let end = rest_str
                        .find(|ch: char| ch.is_whitespace() || ch == '<' || ch == '"' || ch == '\'' || ch == '`')
                        .unwrap_or(rest_str.len());
                    let raw = &rest_str[..end];
                    let trimmed_len = trim_bare_url(raw);
                    let trimmed = &raw[..trimmed_len];
                    let domain_part = trimmed.strip_prefix("www.").unwrap_or("");
                    // Domain must contain at least one more dot and be valid.
                    if let Some(dot) = domain_part.find('.') {
                        let domain = &domain_part[..domain_part.find(|ch: char| ch == '/' || ch == '?' || ch == '#').unwrap_or(domain_part.len())];
                        let _ = dot;
                        if valid_domain(domain) {
                            flush(&mut buf, &mut nodes);
                            nodes.push(INode::Link(vec![INode::Text(trimmed.to_string())]));
                            // Trailing punctuation returns to the buffer.
                            buf.push_str(&raw[trimmed_len..]);
                            let mut adv = i;
                            let mut bi2 = 0usize;
                            while bi2 < end && adv < chars.len() {
                                bi2 += chars[adv].len_utf8();
                                adv += 1;
                            }
                            i = adv;
                            continue;
                        }
                    }
                }
            }
            // Scheme URLs.
            if rest_str.starts_with("http://") || rest_str.starts_with("https://") || rest_str.starts_with("ftp://") {
                let prev_ok = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
                if prev_ok {
                    let end = rest_str
                        .find(|ch: char| ch.is_whitespace() || ch == '<' || ch == '"' || ch == '\'' || ch == '`')
                        .unwrap_or(rest_str.len());
                    let raw = &rest_str[..end];
                    let trimmed_len = trim_bare_url(raw);
                    let trimmed = &raw[..trimmed_len];
                    if !trimmed.is_empty() {
                        flush(&mut buf, &mut nodes);
                        nodes.push(INode::Link(vec![INode::Text(trimmed.to_string())]));
                        buf.push_str(&raw[trimmed_len..]);
                        let mut adv = i;
                        let mut bi2 = 0usize;
                        while bi2 < end && adv < chars.len() {
                            bi2 += chars[adv].len_utf8();
                            adv += 1;
                        }
                        i = adv;
                        continue;
                    }
                }
            }
            // Bare email: scan a word containing '@'.
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '%') {
                let word_end = rest_str
                    .find(|ch: char| ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':'))
                    .unwrap_or(rest_str.len());
                // Email words may contain ':'? No — keep the simple
                // scan: if the word contains '@' and validates, and
                // the previous char is not a word char, link it.
                let word = &rest_str[..word_end];
                let prev_ok = i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == '.');
                if prev_ok && word.contains('@') && valid_bare_email(word) {
                    flush(&mut buf, &mut nodes);
                    nodes.push(INode::Link(vec![INode::Text(word.to_string())]));
                    let mut adv = i;
                    let mut bi2 = 0usize;
                    while bi2 < word_end && adv < chars.len() {
                        bi2 += chars[adv].len_utf8();
                        adv += 1;
                    }
                    i = adv;
                    continue;
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut nodes);
    // Resolve emphasis / strikethrough in place.
    let mut nodes = process_emphasis(nodes);
    // Recursively resolve emphasis inside link texts that were parsed
    // with allow_links=false: their nodes still contain delimiters.
    // parse_inline_nodes already calls process_emphasis at its end, so
    // link inner nodes are resolved — but only if parse_inline_nodes
    // was used for them (it was, via parse_link_at).
    nodes
}

fn node_plain_text(node: &INode) -> Vec<char> {
    match node {
        INode::Empty => Vec::new(),
        INode::Text(t) | INode::Code(t) | INode::Image(t) => t.chars().collect(),
        INode::Link(inner) | INode::Em(inner) | INode::Strong(inner) | INode::Strike(inner) => {
            inner.iter().flat_map(node_plain_text).collect()
        }
        INode::Delim { ch, len, .. } => std::iter::repeat_n(*ch, *len).collect(),
    }
}

/// Emphasis processing (GFM 6.4, rules 9-17, plus strikethrough).
/// Repeatedly finds the first pairable closer, wraps its content,
/// and continues; each wrap reduces total delimiter length, so the
/// loop terminates. The odd-match rule: if either delimiter can both
/// open and close, the sum of the two run lengths must not be a
/// multiple of 3 unless both are multiples of 3.
fn process_emphasis(mut nodes: Vec<INode>) -> Vec<INode> {
    // Single pass, left to right, with in-place takes: wrapping a
    // pair takes its content nodes into the wrapped node and leaves
    // Empty placeholders behind, instead of draining/inserting in
    // the Vec (which shifted O(n) elements per wrap and made long
    // messages quadratic). Each node is visited a constant number
    // of times, so this is linear in the number of inline nodes.
    let mut i = 0usize;
    while i < nodes.len() {
        let (ch, len, can_close, can_open_c) = match &nodes[i] {
            INode::Delim {
                ch,
                len,
                can_open,
                can_close,
            } => (*ch, *len, *can_close, *can_open),
            _ => {
                i += 1;
                continue;
            }
        };
        if !can_close || len == 0 {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut opener: Option<usize> = None;
        while j > 0 {
            j -= 1;
            let (och, olen, ocan_open, ocan_close) = match &nodes[j] {
                INode::Delim {
                    ch,
                    len,
                    can_open,
                    can_close,
                } => (*ch, *len, *can_open, *can_close),
                _ => continue,
            };
            if och != ch || !ocan_open || olen == 0 {
                continue;
            }
            if (ocan_open && ocan_close) || (can_open_c && can_close) {
                if (olen + len) % 3 == 0 && !(olen % 3 == 0 && len % 3 == 0) {
                    continue;
                }
            }
            let has_content = nodes[j + 1..i].iter().any(|n| {
                !matches!(n, INode::Delim { .. } | INode::Empty)
            });
            if !has_content {
                continue;
            }
            opener = Some(j);
            break;
        }
        let Some(op) = opener else {
            i += 1;
            continue;
        };
        let (olen, ocan_open, ocan_close) = match &nodes[op] {
            INode::Delim {
                len,
                can_open,
                can_close,
                ..
            } => (*len, *can_open, *can_close),
            _ => unreachable!(),
        };
        let use_n = if ch == '~' {
            2
        } else if olen >= 2 && len >= 2 {
            2
        } else {
            1
        };
        if ch == '~' && (olen < 2 || len < 2) {
            i += 1;
            continue;
        }
        let cl = i;
        let olen_rem = olen - use_n;
        let clen_rem = len - use_n;
        // Take content nodes in place (no shifts).
        let mut content: Vec<INode> = Vec::new();
        for k in (op + 1)..cl {
            let taken = std::mem::replace(&mut nodes[k], INode::Empty);
            if !matches!(taken, INode::Empty) {
                content.push(taken);
            }
        }
        let wrapped = match ch {
            '~' => INode::Strike(content),
            _ => {
                if use_n == 2 {
                    INode::Strong(content)
                } else {
                    INode::Em(content)
                }
            }
        };
        // Closer slot.
        nodes[cl] = if clen_rem > 0 {
            INode::Delim {
                ch,
                len: clen_rem,
                can_open: can_open_c,
                can_close,
            }
        } else {
            INode::Empty
        };
        if olen_rem == 0 {
            nodes[op] = wrapped;
            i = op + 1;
        } else {
            nodes[op] = INode::Delim {
                ch,
                len: olen_rem,
                can_open: ocan_open,
                can_close: ocan_close,
            };
            // op+1 was the first content slot, now Empty: put wrapped there.
            nodes[op + 1] = wrapped;
            i = op + 2;
        }
    }
    nodes.retain(|n| !matches!(n, INode::Empty));
    nodes
}

/// Flatten an inline node tree to styled spans.
fn flatten_nodes(nodes: &[INode], th: &Theme, out: &mut Vec<Span<'static>>, style: Style) {
    for node in nodes {
        match node {
            INode::Empty => {}
            INode::Text(t) => {
                out.push(Span::styled(t.clone(), style));
            }
            INode::Code(t) => {
                out.push(Span::styled(t.clone(), Style::default().fg(th.code)));
            }
            INode::Link(inner) => {
                let mut inner_spans: Vec<Span<'static>> = Vec::new();
                flatten_nodes(inner, th, &mut inner_spans, Style::default().fg(th.body));
                for mut sp in inner_spans {
                    sp.style.fg = Some(th.primary);
                    out.push(sp);
                }
            }
            INode::Image(alt) => {
                out.push(Span::styled(alt.clone(), Style::default().fg(th.primary)));
            }
            INode::Em(inner) => {
                let mut s = style;
                s.fg = Some(th.emphasis);
                s = s.add_modifier(Modifier::ITALIC);
                flatten_nodes(inner, th, out, s);
            }
            INode::Strong(inner) => {
                let mut s = style;
                s.fg = Some(th.emphasis);
                s = s.add_modifier(Modifier::BOLD);
                flatten_nodes(inner, th, out, s);
            }
            INode::Strike(inner) => {
                let mut s = style;
                s = s.add_modifier(Modifier::CROSSED_OUT);
                flatten_nodes(inner, th, out, s);
            }
            INode::Delim { ch, len, .. } => {
                out.push(Span::styled(
                    std::iter::repeat_n(*ch, *len).collect::<String>(),
                    style,
                ));
            }
        }
    }
}

/// Public inline entry point: text → styled spans.
fn inline(text: &str, th: &Theme, defs: &Defs) -> Vec<Span<'static>> {
    let nodes = parse_inline_nodes(text, th, defs, true);
    let mut out: Vec<Span<'static>> = Vec::new();
    flatten_nodes(&nodes, th, &mut out, Style::default().fg(th.body));
    // Merge consecutive spans with identical styles (keeps wrapping
    // and span counts sane).
    let mut merged: Vec<Span<'static>> = Vec::new();
    for sp in out {
        if let Some(last) = merged.last_mut() {
            if last.style == sp.style {
                let mut content = last.content.to_string();
                content.push_str(&sp.content);
                last.content = content.into();
                continue;
            }
        }
        merged.push(sp);
    }
    merged
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
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(input: &str) -> String {
        text(&render_markdown(input, &theme()))
    }

    fn span_style(input: &str, content: &str) -> Option<Style> {
        // Spans may carry surrounding spaces (e.g. "foo " inside an
        // emphasis run), so match by containment, not exact equality.
        render_markdown(input, &theme())
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref().contains(content))
            .map(|s| s.style)
    }

    fn has_modifier(input: &str, content: &str, m: Modifier) -> bool {
        span_style(input, content)
            .map(|s| s.add_modifier.contains(m))
            .unwrap_or(false)
    }

    // --- previously covered behaviour (regression) ---
    #[test]
    fn strikethrough_drops_tildes_and_marks_crossed_out() {
        let lines = render_markdown("a ~~gone~~ b", &theme());
        assert_eq!(text(&lines), "a gone b");
        assert!(has_modifier("a ~~gone~~ b", "gone", Modifier::CROSSED_OUT));
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
        assert_eq!(render("mail <user@example.com> ok"), "mail user@example.com ok");
    }

    #[test]
    fn generics_are_not_mangled_as_autolinks() {
        assert_eq!(render("use Vec<T> here"), "use Vec<T> here");
    }

    #[test]
    fn table_alignment_and_no_outer_pipes_and_escaped_pipe() {
        let t = render("| Name | Cost |\n| :--- | ---: |\n| ab | 5 |\n| abcdef | 42 |");
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "Name    Cost");
        assert_eq!(lines[1], "ab         5");
        assert_eq!(lines[2], "abcdef    42");
        let t = render("| A | B |\n| :---: | :--- |\n| x | y |");
        assert_eq!(t.lines().next().unwrap(), "A  B");
        let t = render("Name | Cost\n--- | ---:\nfoo | 7");
        assert!(t.contains("foo"), "got: {t}");
        assert!(!t.contains('|'), "got: {t}");
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
    fn wrapped_prose_breaks_at_width_and_loses_no_text() {
        let input = "Sunlight looks white because it is made of all the colours of the rainbow mixed together, and when they reach your eyes at the same time your brain sees them as white light.";
        for width in [80usize, 120] {
            let lines = render_markdown_wrapped(input, &theme(), width);
            assert!(lines.len() > 1, "must wrap at {width}: {lines:?}");
            for l in &lines {
                assert!(l.width() <= width, "line over {width}: [{l}]");
            }
            let joined = lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(joined, input, "wrapped text must not lose words");
        }
    }

    #[test]
    fn wrapped_prose_keeps_styled_spans_and_does_not_wrap_code_or_tables() {
        let input = "The **sky looks blue** because air scatters blue light in every direction, and that scattered blue is what reaches our eyes from all over the sky above us.";
        let lines = render_markdown_wrapped(input, &theme(), 40);
        assert!(lines.len() > 1, "must wrap at 40");
        let bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "sky looks blue")
            .expect("bold span must survive wrapping");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = render_markdown_wrapped("```\nlet very_long_line = some_function_call(with_many_arguments, and_more_arguments, even_more);\n```", &theme(), 40);
        assert_eq!(code.len(), 1, "code must not wrap: {code:?}");
        assert!(code[0].width() > 40, "code line must stay long");
        let table = render_markdown_wrapped("| Name | Cost |\n| --- | --- |\n| averylongname | 12345 |", &theme(), 20);
        assert_eq!(table.len(), 2, "table must not wrap: {table:?}");
        let list = render_markdown_wrapped("- a very long list item that keeps going and going until it must wrap onto another line", &theme(), 40);
        assert!(list.len() > 1, "list must wrap");
        assert!(
            list[1].spans[0].content.starts_with("  "),
            "continuation must indent: {:?}",
            list[1]
        );
        let word = "a".repeat(100);
        let lines = render_markdown_wrapped(&word, &theme(), 80);
        assert_eq!(lines.len(), 2, "long word must split: {lines:?}");
        assert_eq!(
            lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<String>(),
            word
        );
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

    // --- GFM 4.2/4.3 headings ---
    #[test]
    fn atx_empty_and_non_headings() {
        assert_eq!(render("#"), "");
        assert_eq!(render("# "), "");
        assert_eq!(render("####### foo"), "####### foo");
        assert_eq!(render("#foo"), "#foo");
        assert_eq!(render("## foo #"), "foo");
        assert_eq!(render("# **bold** title"), "bold title");
        assert!(has_modifier("# **bold** title", "bold", Modifier::BOLD));
    }

    #[test]
    fn setext_with_inline_and_trailing_spaces() {
        assert_eq!(render("Foo *bar*\n===\nbody"), "Foo bar\nbody");
        assert_eq!(render("Foo\n===   \nbody"), "Foo\nbody");
    }

    // --- GFM 4.4/4.5 code blocks ---
    #[test]
    fn indented_code_blocks() {
        let t = render("para\n\n    code line\n    more code\n\nnext");
        assert!(t.contains("  code line"), "got: {t}");
        assert!(t.contains("  more code"), "got: {t}");
        // Indented code cannot interrupt a paragraph (the first line
        // is >48 chars so it is not a Pantheon section header).
        let t = render("a long paragraph line that is definitely longer than forty eight characters\n    not code");
        assert_eq!(t, "a long paragraph line that is definitely longer than forty eight characters\nnot code");
    }

    #[test]
    fn tilde_fences_and_fence_length_rules() {
        let t = render("~~~\ncode\n~~~");
        assert!(t.contains("  code"), "got: {t}");
        assert!(!t.contains('~'), "got: {t}");
        // A 3-backtick line does not close a 4-backtick fence.
        let t = render("````\n```\ncode\n````");
        assert!(t.contains("  ```"), "got: {t}");
        assert!(t.contains("  code"), "got: {t}");
        // Backticks forbidden in a backtick fence info string.
        let t = render("```a`b\ncode\n```");
        assert!(t.contains("```a`b"), "got: {t}");
    }

    // --- GFM 4.6/4.7 HTML blocks and link refs ---
    #[test]
    fn html_blocks_render_literally() {
        let t = render("<div>\n*foo*\n\npara");
        assert!(t.contains("*foo*"), "got: {t}");
        assert!(!has_modifier("<div>\n*foo*", "foo", Modifier::ITALIC));
        let t = render("<script>\nalert(1)\n</script>");
        assert!(t.contains("<script>"), "got: {t}");
    }

    #[test]
    fn link_reference_definitions() {
        let t = render("[foo]: /url \"title\"\n\n[foo] and [bar][foo] and [foo][] and [FOO]");
        assert_eq!(t, "foo and bar and foo and FOO");
        assert!(!t.contains("/url"), "got: {t}");
        // First definition wins.
        let t = render("[foo]: /one\n[foo]: /two\n\n[foo]");
        assert_eq!(t, "foo");
        // Collapsed label with spaces / case folding.
        let t = render("[my link]: /u\n\n[My  Link]");
        assert_eq!(t, "My  Link");
    }

    // --- minor-e: GFM remaining gaps ---
    #[test]
    fn html_comment_runs_to_closing_marker_not_blank_line() {
        // A comment block ends at `-->`; markdown after it parses.
        let input = "<!--\nnote\n-->\n*emph*\n";
        assert!(
            has_modifier(input, "emph", Modifier::ITALIC),
            "emphasis after a comment must parse: {}",
            render(input)
        );
        let input = "<![CDATA[\nx\n]]>\n*emph*\n";
        assert!(
            has_modifier(input, "emph", Modifier::ITALIC),
            "emphasis after CDATA must parse: {}",
            render(input)
        );
        let input = "<?pi\nmore?>\n*emph*\n";
        assert!(
            has_modifier(input, "emph", Modifier::ITALIC),
            "emphasis after a PI must parse: {}",
            render(input)
        );
    }

    #[test]
    fn multiline_link_reference_definitions() {
        // Destination on the next line.
        let t = render("[foo]:\n/url\n\n[foo]");
        assert_eq!(t, "foo");
        // Title alone on the next line belongs to the definition.
        let t = render("[foo]: /url\n\"Title\"\n\n[foo]");
        assert_eq!(t, "foo");
        assert!(!t.contains("Title"), "title line must be consumed: {t}");
        // Destination and title each on their own line.
        let t = render("[foo]:\n/url\n\"Title\"\n\n[foo]");
        assert_eq!(t, "foo");
        assert!(!t.contains("Title"), "title line must be consumed: {t}");
    }

    #[test]
    fn def_inside_html_block_is_not_collected() {
        let t = render("<div>\n[foo]: /url\n</div>\n\n[foo]");
        assert!(t.contains("[foo]"), "unresolved ref must stay literal: {t}");
    }

    #[test]
    fn bare_email_requires_dotted_domain_but_angle_autolink_does_not() {
        let input = "mail foo@bar and bar@baz.com end";
        let style_plain = span_style(input, "foo@bar").expect("foo@bar span");
        assert_eq!(
            style_plain.fg,
            Some(theme().body),
            "foo@bar must not link (no dot in domain)"
        );
        let style_link = span_style(input, "bar@baz.com").expect("bar@baz.com span");
        assert_eq!(
            style_link.fg,
            Some(theme().primary),
            "bar@baz.com must link"
        );
        // Angle autolinks keep the spec's dot-less email pattern.
        let style_angle = span_style("<foo@bar>", "foo@bar").expect("angle span");
        assert_eq!(style_angle.fg, Some(theme().primary));
    }

    #[test]
    fn bare_url_trailing_entity_semicolon_is_kept() {
        // A stripped ';' still renders (as trailing text outside the
        // link), so assert on the link span itself, not the joined text.
        let link_span = |input: &str| {
            render_markdown(input, &theme())
                .into_iter()
                .flat_map(|l| l.spans)
                .find(|s| s.content.contains("example.com"))
                .expect("link span")
        };
        let link = link_span("see https://example.com/?a=1&amp; end");
        assert!(
            link.content.ends_with("&amp;"),
            "entity semicolon must stay in the link: {}",
            link.content
        );
        // A plain trailing ';' is still stripped from the link.
        let link = link_span("see https://example.com/a; end");
        assert_eq!(link.content.as_ref(), "https://example.com/a");
    }

    #[test]
    fn tab_expansion_covers_interior_tabs() {
        assert_eq!(render("a\tb"), "a   b");
        let t = render("\tcode");
        assert!(t.contains("  code"), "tab-indented code must render: {t}");
    }

    #[test]
    fn section_header_ignores_short_sentences() {
        let input = "\nShort sentence here.\nmore text";
        assert!(
            !has_modifier(input, "Short sentence here.", Modifier::BOLD),
            "a sentence must not render as a heading: {}",
            render(input)
        );
        let input = "\nShort heading\nmore text";
        assert!(
            has_modifier(input, "Short heading", Modifier::BOLD),
            "a bare short line still renders as a heading: {}",
            render(input)
        );
    }

    #[test]
    fn underscore_emphasis_ignores_identifiers() {
        assert!(!has_modifier("foo_bar_baz", "bar", Modifier::ITALIC));
        assert_eq!(render("foo_bar_baz"), "foo_bar_baz");
    }

    // --- GFM 4.10 tables with inline ---
    #[test]
    fn table_cells_have_inline_formatting() {
        let t = render("| **A** | B |\n| --- | --- |\n| *x* | `y` |");
        assert!(t.contains("A"), "got: {t}");
        assert!(t.contains("x"), "got: {t}");
        assert!(t.contains("y"), "got: {t}");
        assert!(has_modifier("| **A** | B |\n| --- | --- |\n| *x* | `y` |", "x", Modifier::ITALIC));
    }

    // --- GFM 5.1 block quotes ---
    #[test]
    fn blockquote_nested_and_lazy_continuation() {
        let t = render("> > nested\n> outer");
        assert!(t.contains("> > nested"), "got: {t}");
        assert!(t.contains("> outer"), "got: {t}");
        let t = render("> quote\ncontinuation");
        assert!(t.contains("> quote"), "got: {t}");
        assert!(t.contains("> continuation"), "got: {t}");
    }

    // --- GFM 5.2/5.4 lists ---
    #[test]
    fn ordered_paren_markers_and_start_numbers() {
        let t = render("1) one\n2) two");
        assert!(t.contains("1) one"), "got: {t}");
        assert!(t.contains("2) two"), "got: {t}");
        let t = render("3. three\n4. four");
        assert!(t.contains("3. three"), "got: {t}");
        // Ordered list starting at 3 cannot interrupt a paragraph.
        let t = render("para\n3. three");
        assert_eq!(t, "para\n3. three");
        // ...but starting at 1 can.
        let t = render("para\n1. one");
        assert!(t.contains("1. one"), "got: {t}");
    }

    #[test]
    fn list_continuation_and_loose_lists() {
        let t = render("- one\n  continued line");
        assert!(t.contains("- one"), "got: {t}");
        assert!(t.contains("  continued line"), "got: {t}");
        let t = render("- a\n\n- b");
        assert_eq!(t, "- a\n\n- b");
    }

    // --- GFM 6.2 entities ---
    #[test]
    fn entities_and_numeric_references() {
        assert_eq!(render("a &amp; b &copy; c"), "a & b © c");
        assert_eq!(render("&#35; &#x22; &#X41;"), "# \" A");
        assert_eq!(render("&nosuchentity; &amp"), "&nosuchentity; &amp");
        // Entities are not decoded in code spans.
        assert_eq!(render("`&amp;`"), "&amp;");
    }

    // --- GFM 6.3 code spans ---
    #[test]
    fn code_span_runs_and_stripping() {
        assert_eq!(render("``` code ```"), "code");
        assert_eq!(render("` code `"), "code");
        assert_eq!(render("`` ` ``"), "`");
        assert_eq!(render("`unclosed"), "`unclosed");
    }

    // --- Multi-line code spans: parse-time joining only --------------
    #[test]
    fn code_span_joins_only_unclosed_runs() {
        assert!(unclosed_backtick_run("`foo"));
        assert!(!unclosed_backtick_run("`foo`"));
        assert!(!unclosed_backtick_run("`` ` ``"));
        assert!(!unclosed_backtick_run("a \\` b"));
        // The span closes on the next line: backticks drop, both
        // halves keep code styling, and the line break stays put —
        // no re-flow, no paragraph joining. (The first line is long
        // on purpose: a short first line is a section header, which
        // parses inline per line and never reaches the body join.)
        assert_eq!(
            render("please consider the following multi-line example `code\nmore` here"),
            "please consider the following multi-line example code\nmore here"
        );
        let th = theme();
        for part in ["code", "more"] {
            let s = span_style(
                "please consider the following multi-line example `code\nmore` here",
                part,
            )
            .unwrap();
            assert_eq!(s.fg, Some(th.code), "{part} must stay code-styled");
        }
        assert_eq!(render("`foo\nbar`"), "foo\nbar");
        // Never closed inside the paragraph: both lines render
        // exactly as before, backticks literal.
        assert_eq!(render("`unclosed\nnext line"), "`unclosed\nnext line");
        // A block start on the next line is never swallowed: the
        // heading still renders as a heading, the span stays literal.
        assert_eq!(render("`foo\n# heading"), "`foo\nheading");
        // An escaped backtick does not open a span, so no join.
        assert_eq!(render("a \\` b\nc `d`"), "a ` b\nc d");
    }

    // --- GFM 6.4 emphasis ---
    #[test]
    fn emphasis_intraword_rules() {
        assert_eq!(render("foo_bar_baz"), "foo_bar_baz");
        assert!(!has_modifier("foo_bar_baz", "bar", Modifier::ITALIC));
        assert_eq!(render("a*b*c"), "abc");
        assert!(has_modifier("a*b*c", "b", Modifier::ITALIC));
        assert_eq!(render("_foo_bar"), "_foo_bar");
        // Intraword __ cannot open/close: literal (GFM 6.4).
        assert_eq!(render("__foo__bar"), "__foo__bar");
        assert!(!has_modifier("__foo__bar", "foo", Modifier::BOLD));
    }

    #[test]
    fn emphasis_nesting_and_triple() {
        assert_eq!(render("***foo***"), "foo");
        let s = span_style("***foo***", "foo").unwrap();
        assert!(s.add_modifier.contains(Modifier::BOLD) && s.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(render("*foo **bar** baz*"), "foo bar baz");
        assert!(has_modifier("*foo **bar** baz*", "bar", Modifier::BOLD));
        assert!(has_modifier("*foo **bar** baz*", "foo", Modifier::ITALIC));
        // foo***bar***baz renders as foo + bar (em+strong) + baz.
        assert_eq!(render("foo***bar***baz"), "foobarbaz");
        assert_eq!(render("**foo*"), "*foo");
        assert!(has_modifier("**foo*", "foo", Modifier::ITALIC));
        assert_eq!(render("*foo**"), "foo*");
        assert!(has_modifier("*foo**", "foo", Modifier::ITALIC));
        assert_eq!(render("*foo**bar*"), "foo**bar");
        assert_eq!(render("** is not an empty emphasis"), "** is not an empty emphasis");
    }

    // --- GFM 6.5 strikethrough ---
    #[test]
    fn strikethrough_variants() {
        assert_eq!(render("~~Hi~~ Hello"), "Hi Hello");
        assert!(has_modifier("~~Hi~~ Hello", "Hi", Modifier::CROSSED_OUT));
        assert_eq!(render("~Hi~"), "~Hi~");
    }

    // --- GFM 6.6/6.7 links and images ---
    #[test]
    fn links_inline_reference_and_titles() {
        assert_eq!(render("[text](/url \"title\")"), "text");
        assert_eq!(render("[text](</my url>)"), "text");
        assert_eq!(render("[text](/url(a)b)"), "text");
        assert_eq!(render("[*em*](/url)"), "em");
        assert!(has_modifier("[*em*](/url)", "em", Modifier::ITALIC));
        assert_eq!(render("![alt](/img.png)"), "alt");
        assert_eq!(render("[img]: /i.png\n\n![alt][img]"), "alt");
        assert_eq!(render("[a [b](/u2)](/u1)"), "a [b](/u2)");
    }

    // --- GFM 6.8/6.9 autolinks ---
    #[test]
    fn autolink_extension_www_email_and_parens() {
        // Trailing '.' is not part of the link, but stays as text.
        assert_eq!(render("see www.example.com/path."), "see www.example.com/path.");
        let lines = render_markdown("see www.example.com/path.", &theme());
        let link = lines[0].spans.iter().find(|sp| sp.content.as_ref() == "www.example.com/path");
        assert!(link.is_some(), "www link span missing");
        assert_eq!(render("mail foo@bar.baz end"), "mail foo@bar.baz end");
        let lines = render_markdown("see https://example.com/a_(b).", &theme());
        assert_eq!(text(&lines), "see https://example.com/a_(b).");
        assert_eq!(render("<foo@bar>"), "foo@bar");
        assert_eq!(render("<foo@bar.baz>"), "foo@bar.baz");
    }

    // --- GFM 6.1 escapes ---
    #[test]
    fn backslash_escapes() {
        // Backslash before a non-punctuation char stays literal.
        assert_eq!(render("\\a \\* \\| \\\\"), "\\a * | \\");
        assert_eq!(render("\\[not a link](x)"), "[not a link](x)");
    }

    // --- performance ---
    #[test]
    fn large_readme_renders_quickly() {
        let para = "The **sky** looks *blue* because air scatters light, see [docs](https://example.com) and `code` and www.example.com/path with ~~struck~~ text. ";
        let input = para.repeat(2000);
        let start = std::time::Instant::now();
        let lines = render_markdown(&input, &theme());
        let elapsed = start.elapsed();
        assert!(!lines.is_empty());
        assert!(
            elapsed.as_millis() < 5000,
            "rendering {} chars took {:?}",
            input.len(),
            elapsed
        );
        eprintln!("large render: {} chars in {:?}", input.len(), elapsed);
    }








}
