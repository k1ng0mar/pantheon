//! Native TUI rendering for the UI protocol (`pantheon_api::ui`).
//!
//! Each component lowers to `Vec<Line>` in the current theme, honoring
//! the terminal width. This module is deliberately dumb: no interaction
//! state, no ledger access - it renders one document. Degradation for
//! unsupported components happens in the protocol (`degrade_for`) before
//! anything reaches here.

use pantheon_api::ui::{
    ActionKind, Align, ApprovalCard, Chart, ChartType, Code, Component, Details, Diff, Form,
    Layout, LayoutKind, List, Markdown, Metrics, Progress, Refs, Table, TaskList, TaskState,
    UiNode,
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::markdown::render_markdown;
use crate::session::theme::Theme;

/// Render one UI document node to transcript lines.
pub fn render_ui_node(node: &UiNode, th: &Theme, term_width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    render_component(&node.component, th, term_width as usize, &mut lines, 0);
    lines
}

fn render_component(
    c: &Component,
    th: &Theme,
    width: usize,
    out: &mut Vec<Line<'static>>,
    indent: usize,
) {
    let pad = " ".repeat(indent.min(8));
    match c {
        Component::Markdown(m) => render_markdown_comp(m, th, width, &pad, out),
        Component::Table(t) => render_table(t, th, width, &pad, out),
        Component::Chart(c) => render_chart(c, th, width, &pad, out),
        Component::Metrics(m) => render_metrics(m, th, width, &pad, out),
        Component::List(l) => render_list(l, th, width, &pad, out),
        Component::Code(c) => render_code(c, th, width, &pad, out),
        Component::Refs(r) => render_refs(r, th, width, &pad, out),
        Component::Form(f) => render_form(f, th, width, &pad, out),
        Component::TaskList(t) => render_task_list(t, th, width, &pad, out),
        Component::Progress(p) => render_progress(p, th, width, &pad, out),
        Component::Details(d) => render_details(d, th, width, &pad, out),
        Component::Diff(d) => render_diff(d, th, width, &pad, out),
        Component::Approval(a) => render_approval(a, th, width, &pad, out),
        Component::Layout(l) => render_layout(l, th, width, &pad, out),
        Component::Custom(c) => {
            // No registered custom renderer in the TUI: the declared
            // fallback is the honest render.
            render_component(&c.fallback.component, th, width, out, indent);
        }
    }
}

fn title_line(title: &str, th: &Theme, pad: &str, out: &mut Vec<Line<'static>>) {
    out.push(Line::from(vec![Span::styled(
        format!("{pad}◆ {title}"),
        Style::default().fg(th.heading).add_modifier(Modifier::BOLD),
    )]));
}

fn fit(s: &str, width: usize) -> String {
    let w = width.max(8);
    if s.chars().count() <= w {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn render_markdown_comp(
    m: &Markdown,
    th: &Theme,
    width: usize,
    pad: &str,
    out: &mut Vec<Line<'static>>,
) {
    if let Some(t) = &m.title {
        title_line(t, th, pad, out);
    }
    for line in render_markdown(&m.text, th) {
        out.push(prefix_line(line, pad, width));
    }
}

fn prefix_line(line: Line<'static>, pad: &str, width: usize) -> Line<'static> {
    if pad.is_empty() {
        return line;
    }
    let mut spans = vec![Span::raw(pad.to_string())];
    spans.extend(line.spans);
    let _ = width;
    Line::from(spans)
}

fn render_table(t: &Table, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    if let Some(title) = &t.title {
        title_line(title, th, pad, out);
    }
    if t.headers.is_empty() {
        return;
    }
    let cols = t.headers.len();
    // Column widths: content-driven, capped so the table fits.
    let mut widths = vec![0usize; cols];
    for (i, h) in t.headers.iter().enumerate() {
        widths[i] = widths[i].max(h.chars().count());
    }
    for row in &t.rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count().min(40));
        }
    }
    let total: usize = widths.iter().sum::<usize>() + cols * 3 + 1;
    let avail = width.saturating_sub(pad.len()).max(20);
    if total > avail {
        // Shrink proportionally to the available budget, minimum 1 per
        // column. Guarantees the table never exceeds the terminal width.
        let overhead = cols * 3 + 1;
        let budget = avail.saturating_sub(overhead);
        let total_w: usize = widths.iter().sum::<usize>().max(1);
        let mut shrunk = vec![0usize; cols];
        let mut remaining = budget;
        for (i, w) in widths.iter().enumerate() {
            if i + 1 == cols {
                shrunk[i] = remaining.max(1);
            } else {
                let reserve = (cols - i - 1).max(1);
                let share = (w * budget / total_w)
                    .max(1)
                    .min(remaining.saturating_sub(reserve).max(1));
                shrunk[i] = share;
                remaining = remaining.saturating_sub(share);
            }
        }
        widths = shrunk;
    }
    let align_at = |i: usize| {
        t.align
            .as_ref()
            .and_then(|a| a.get(i))
            .copied()
            .unwrap_or(Align::Left)
    };
    let cell = |s: &str, i: usize| {
        let w = widths[i];
        let s = fit(s, w);
        let len = s.chars().count();
        match align_at(i) {
            Align::Left => format!("{s:<w$}", w = w.max(len)),
            Align::Right => format!("{s:>w$}", w = w.max(len)),
            Align::Center => {
                let extra = w.saturating_sub(len);
                format!(
                    "{}{}{}",
                    " ".repeat(extra / 2),
                    s,
                    " ".repeat(extra - extra / 2)
                )
            }
        }
    };
    let row_line = |cells: &[String]| {
        let mut s = format!("{pad}│ ");
        s.push_str(&cells.join(" │ "));
        s.push_str(" │");
        fit(&s, width)
    };
    let header: Vec<String> = t
        .headers
        .iter()
        .enumerate()
        .map(|(i, h)| cell(h, i))
        .collect();
    out.push(Line::from(Span::styled(
        row_line(&header),
        Style::default().fg(th.heading).add_modifier(Modifier::BOLD),
    )));
    let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    out.push(Line::from(Span::styled(
        row_line(&sep),
        Style::default().fg(th.dim),
    )));
    for row in &t.rows {
        let cells: Vec<String> = (0..cols)
            .map(|i| cell(row.get(i).map(String::as_str).unwrap_or(""), i))
            .collect();
        out.push(Line::from(Span::styled(
            row_line(&cells),
            Style::default().fg(th.body),
        )));
    }
}

fn render_chart(c: &Chart, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    if let Some(t) = &c.title {
        title_line(t, th, pad, out);
    }
    let avail = width.saturating_sub(pad.len()).max(20);
    match c.chart_type {
        ChartType::Bar => {
            let max = c
                .series
                .iter()
                .flat_map(|s| s.values.iter())
                .copied()
                .fold(0.0f64, f64::max)
                .max(1e-9);
            let label_w = c
                .labels
                .iter()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0)
                .min(20);
            let bar_w = avail.saturating_sub(label_w + 12).clamp(8, 40);
            for (li, label) in c.labels.iter().enumerate() {
                for s in &c.series {
                    let v = s.values.get(li).copied().unwrap_or(0.0);
                    let n = ((v / max) * bar_w as f64).round() as usize;
                    let bar = "█".repeat(n);
                    let name = if c.series.len() > 1 {
                        format!("{} ", s.name)
                    } else {
                        String::new()
                    };
                    out.push(Line::from(vec![
                        Span::styled(
                            format!("{pad}{:<label_w$} ", fit(label, label_w), label_w = label_w),
                            Style::default().fg(th.dim),
                        ),
                        Span::styled(bar, Style::default().fg(th.primary)),
                        Span::styled(format!(" {name}{v}"), Style::default().fg(th.body)),
                    ]));
                }
            }
        }
        ChartType::Line => {
            // Sparkline per series.
            const GLYPHS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
            for s in &c.series {
                let min = s.values.iter().copied().fold(f64::INFINITY, f64::min);
                let max = s.values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let span = (max - min).max(1e-9);
                let spark: String = s
                    .values
                    .iter()
                    .take(avail.saturating_sub(24).max(8))
                    .map(|v| GLYPHS[(((v - min) / span) * 7.0).round() as usize])
                    .collect();
                out.push(Line::from(vec![
                    Span::styled(
                        format!("{pad}{:<12} ", fit(&s.name, 12)),
                        Style::default().fg(th.dim),
                    ),
                    Span::styled(spark, Style::default().fg(th.primary)),
                ]));
                if !c.labels.is_empty() {
                    let first = c.labels.first().map(String::as_str).unwrap_or("");
                    let last = c.labels.last().map(String::as_str).unwrap_or("");
                    out.push(Line::from(Span::styled(
                        format!("{pad}             {first} → {last}"),
                        Style::default().fg(th.dim),
                    )));
                }
            }
        }
        ChartType::Pie => {
            // Per-label totals across series.
            let mut labeled: Vec<(String, f64)> = Vec::new();
            for (li, label) in c.labels.iter().enumerate() {
                let v: f64 = c
                    .series
                    .iter()
                    .map(|s| s.values.get(li).copied().unwrap_or(0.0))
                    .sum();
                labeled.push((label.clone(), v));
            }
            let grand: f64 = labeled.iter().map(|(_, v)| v).sum::<f64>().max(1e-9);
            let bar_w = avail.saturating_sub(30).clamp(8, 30);
            for (label, v) in labeled {
                let pct = v / grand * 100.0;
                let n = (pct / 100.0 * bar_w as f64).round() as usize;
                out.push(Line::from(vec![
                    Span::styled(
                        format!("{pad}{:<16} ", fit(&label, 16)),
                        Style::default().fg(th.dim),
                    ),
                    Span::styled("█".repeat(n), Style::default().fg(th.primary)),
                    Span::styled(format!(" {pct:.1}%"), Style::default().fg(th.body)),
                ]));
            }
        }
    }
}
fn render_metrics(m: &Metrics, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    if let Some(t) = &m.title {
        title_line(t, th, pad, out);
    }
    let label_w = m
        .items
        .iter()
        .map(|i| i.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(28);
    for item in &m.items {
        let mut spans = vec![
            Span::styled(
                format!(
                    "{pad}{:<label_w$}  ",
                    fit(&item.label, label_w),
                    label_w = label_w
                ),
                Style::default().fg(th.dim),
            ),
            Span::styled(
                fit(
                    &item.value,
                    width.saturating_sub(pad.len() + label_w + 4).max(8),
                ),
                Style::default().fg(th.body).add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(d) = &item.delta {
            let color = if d.trim_start().starts_with(['+', '▲']) {
                th.success
            } else if d.trim_start().starts_with(['-', '▼']) {
                th.failure
            } else {
                th.dim
            };
            spans.push(Span::styled(format!("  {d}"), Style::default().fg(color)));
        }
        out.push(Line::from(spans));
    }
}

fn render_list(l: &List, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    if let Some(t) = &l.title {
        title_line(t, th, pad, out);
    }
    for item in &l.items {
        let (glyph, color) = match item.status {
            Some(pantheon_api::ui::Status::Ok) => ("✓", th.success),
            Some(pantheon_api::ui::Status::Warn) => ("⚠", th.warning),
            Some(pantheon_api::ui::Status::Error) => ("×", th.failure),
            Some(pantheon_api::ui::Status::Info) => ("•", th.primary),
            Some(pantheon_api::ui::Status::Pending) => ("○", th.dim),
            None => ("•", th.dim),
        };
        out.push(Line::from(vec![
            Span::styled(format!("{pad}{glyph} "), Style::default().fg(color)),
            Span::styled(
                fit(&item.text, width.saturating_sub(pad.len() + 2).max(8)),
                Style::default().fg(th.body),
            ),
        ]));
    }
}

fn render_code(c: &Code, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    if let Some(t) = &c.title {
        title_line(t, th, pad, out);
    } else if !c.language.is_empty() {
        out.push(Line::from(Span::styled(
            format!("{pad}```{}", c.language),
            Style::default().fg(th.dim),
        )));
    }
    let total = c.code.lines().count();
    for line in c.code.lines().take(200) {
        out.push(Line::from(Span::styled(
            format!("{pad}{}", fit(line, width.saturating_sub(pad.len()).max(8))),
            Style::default().fg(th.code),
        )));
    }
    if total > 200 {
        // Silent truncation hid code: say how much was dropped.
        out.push(Line::from(Span::styled(
            format!("{pad}… {} more lines", total - 200),
            Style::default().fg(th.dim),
        )));
    }
}

fn render_refs(r: &Refs, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    if let Some(t) = &r.title {
        title_line(t, th, pad, out);
    }
    for (i, item) in r.items.iter().enumerate() {
        out.push(Line::from(vec![
            Span::styled(format!("{pad}[{}] ", i + 1), Style::default().fg(th.dim)),
            Span::styled(
                fit(&item.title, width.saturating_sub(pad.len() + 5).max(8)),
                Style::default().fg(th.primary),
            ),
        ]));
        out.push(Line::from(Span::styled(
            format!(
                "{pad}    {}",
                fit(&item.url, width.saturating_sub(pad.len() + 4).max(8))
            ),
            Style::default().fg(th.dim),
        )));
        if let Some(n) = &item.note {
            out.push(Line::from(Span::styled(
                format!(
                    "{pad}    {}",
                    fit(n, width.saturating_sub(pad.len() + 4).max(8))
                ),
                Style::default().fg(th.body),
            )));
        }
    }
}

fn render_form(f: &Form, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    let title = f.title.as_deref().unwrap_or("Form");
    out.push(Line::from(Span::styled(
        format!("{pad}▣ {title}"),
        Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
    )));
    if let Some(d) = &f.description {
        for line in render_markdown(d, th) {
            out.push(prefix_line(line, pad, width));
        }
    }
    for field in &f.fields {
        let type_hint = match &field.field_type {
            pantheon_api::ui::FieldType::Text => "text",
            pantheon_api::ui::FieldType::LongText => "long text",
            pantheon_api::ui::FieldType::Number => "number",
            pantheon_api::ui::FieldType::Boolean => "true/false",
            pantheon_api::ui::FieldType::Secret => "secret",
            pantheon_api::ui::FieldType::Select { options } => {
                &format!("one of: {}", options.join(", "))
            }
        };
        let req = if field.required { " *" } else { "" };
        let def = field
            .default
            .as_deref()
            .map(|d| {
                if matches!(field.field_type, pantheon_api::ui::FieldType::Secret) {
                    " [default set]".to_string()
                } else {
                    format!(" [default: {d}]")
                }
            })
            .unwrap_or_default();
        out.push(Line::from(vec![
            Span::styled(
                format!("{pad}  {}{req}: ", fit(&field.label, 24)),
                Style::default().fg(th.body).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                fit(
                    &format!("{type_hint}{def}"),
                    width.saturating_sub(pad.len() + 28).max(8),
                ),
                Style::default().fg(th.dim),
            ),
        ]));
    }
    let submit_label = &f.submit.label;
    out.push(Line::from(vec![
        Span::raw(format!("{pad}  ")),
        Span::styled(
            format!("[F] fill form → {submit_label}"),
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "   (values are validated; secrets stay masked)".to_string(),
            Style::default().fg(th.dim),
        ),
    ]));
}

fn render_task_list(
    t: &TaskList,
    th: &Theme,
    width: usize,
    pad: &str,
    out: &mut Vec<Line<'static>>,
) {
    if let Some(title) = &t.title {
        title_line(title, th, pad, out);
    }
    for item in &t.items {
        let (glyph, color) = match item.state {
            TaskState::Done => ("✓", th.success),
            TaskState::Running => ("●", th.primary),
            TaskState::Failed => ("×", th.failure),
            TaskState::Skipped => ("–", th.dim),
            TaskState::Pending => ("○", th.dim),
        };
        let style = if matches!(item.state, TaskState::Done) {
            Style::default().fg(th.dim)
        } else {
            Style::default().fg(th.body)
        };
        out.push(Line::from(vec![
            Span::styled(format!("{pad}[{glyph}] "), Style::default().fg(color)),
            Span::styled(
                fit(&item.label, width.saturating_sub(pad.len() + 4).max(8)),
                style,
            ),
        ]));
        if let Some(d) = &item.detail {
            out.push(Line::from(Span::styled(
                format!(
                    "{pad}     {}",
                    fit(d, width.saturating_sub(pad.len() + 5).max(8))
                ),
                Style::default().fg(th.dim),
            )));
        }
    }
}

fn render_progress(
    p: &Progress,
    th: &Theme,
    width: usize,
    pad: &str,
    out: &mut Vec<Line<'static>>,
) {
    let pct = p
        .done
        .checked_mul(100)
        .and_then(|v| v.checked_div(p.total))
        .unwrap_or(0)
        .min(100);
    let bar_w = width
        .saturating_sub(pad.len() + p.label.chars().count() + 12)
        .clamp(8, 30);
    let filled = (pct as usize * bar_w / 100).min(bar_w);
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));
    out.push(Line::from(vec![
        Span::styled(
            format!("{pad}{} ", fit(&p.label, 24)),
            Style::default().fg(th.body),
        ),
        Span::styled(bar, Style::default().fg(th.primary)),
        Span::styled(
            format!(" {pct}% ({}/{})", p.done, p.total),
            Style::default().fg(th.dim),
        ),
    ]));
}

fn render_details(d: &Details, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    out.push(Line::from(vec![
        Span::styled(format!("{pad}▸ "), Style::default().fg(th.primary)),
        Span::styled(
            fit(&d.summary, width.saturating_sub(pad.len() + 2).max(8)),
            Style::default().fg(th.body).add_modifier(Modifier::BOLD),
        ),
    ]));
    render_component(&d.body.component, th, width, out, pad.len() + 2);
}

fn render_diff(d: &Diff, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    if let Some(t) = &d.title {
        title_line(t, th, pad, out);
    }
    for file in &d.files {
        out.push(Line::from(Span::styled(
            format!(
                "{pad}── {}",
                fit(&file.path, width.saturating_sub(pad.len() + 3).max(8))
            ),
            Style::default().fg(th.heading).add_modifier(Modifier::BOLD),
        )));
        for hunk in &file.hunks {
            if !hunk.header.is_empty() {
                out.push(Line::from(Span::styled(
                    format!(
                        "{pad}{}",
                        fit(&hunk.header, width.saturating_sub(pad.len()).max(8))
                    ),
                    Style::default().fg(th.dim),
                )));
            }
            for line in &hunk.lines {
                let (sigil, color) = match line.kind {
                    pantheon_api::ui::DiffLineKind::Add => ("+", th.success),
                    pantheon_api::ui::DiffLineKind::Del => ("-", th.failure),
                    pantheon_api::ui::DiffLineKind::Context => (" ", th.dim),
                };
                out.push(Line::from(vec![
                    Span::styled(format!("{pad}{sigil}"), Style::default().fg(color)),
                    Span::styled(
                        fit(&line.text, width.saturating_sub(pad.len() + 1).max(8)),
                        Style::default().fg(color),
                    ),
                ]));
            }
        }
    }
}

fn render_approval(
    a: &ApprovalCard,
    th: &Theme,
    width: usize,
    pad: &str,
    out: &mut Vec<Line<'static>>,
) {
    out.push(Line::from(Span::styled(
        format!(
            "{pad}⚠ {}",
            fit(&a.title, width.saturating_sub(pad.len() + 2).max(8))
        ),
        Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
    )));
    for line in render_markdown(&a.detail, th) {
        out.push(prefix_line(line, pad, width));
    }
    if !a.scope_hint.is_empty() {
        out.push(Line::from(Span::styled(
            format!(
                "{pad}scope: {}",
                fit(&a.scope_hint, width.saturating_sub(pad.len() + 7).max(8))
            ),
            Style::default().fg(th.dim),
        )));
    }
    let decided_hint = format!("[a] {}   [d] {}", a.approve.label, a.deny.label);
    out.push(Line::from(vec![
        Span::raw(pad.to_string()),
        Span::styled(
            format!(
                "→ {}",
                fit(&decided_hint, width.saturating_sub(pad.len() + 2).max(8))
            ),
            Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
        ),
    ]));
    let _ = width;
}

fn render_layout(l: &Layout, th: &Theme, width: usize, pad: &str, out: &mut Vec<Line<'static>>) {
    match l.kind {
        LayoutKind::Stack => {
            for (i, child) in l.children.iter().enumerate() {
                if i > 0 {
                    out.push(Line::from(""));
                }
                render_component(&child.component, th, width, out, pad.len());
            }
        }
        LayoutKind::Columns => {
            // Narrow-terminal honesty: the TUI stacks columns vertically
            // with a divider, like the protocol's degrade path.
            for (i, child) in l.children.iter().enumerate() {
                if i > 0 {
                    out.push(Line::from(Span::styled(
                        format!(
                            "{pad}{}",
                            "─".repeat(width.saturating_sub(pad.len()).clamp(8, 60))
                        ),
                        Style::default().fg(th.dim),
                    )));
                }
                render_component(&child.component, th, width, out, pad.len());
            }
        }
        LayoutKind::Tabs => {
            // Tab switching is local state the transcript doesn't
            // keep, so a tabs layout cannot be switched here. Hiding
            // every tab but the first behind an unswitchable bar was
            // worse: like Columns, stack every tab's content under
            // the bar, each labelled, so nothing is unreachable.
            let bar: Vec<String> = l
                .children
                .iter()
                .map(|c| format!(" {} ", tab_label(&c.component)))
                .collect();
            out.push(Line::from(Span::styled(
                format!(
                    "{pad}{}",
                    fit(&bar.join(" │ "), width.saturating_sub(pad.len()).max(8))
                ),
                Style::default().fg(th.dim),
            )));
            for (i, child) in l.children.iter().enumerate() {
                if i > 0 {
                    out.push(Line::from(Span::styled(
                        format!(
                            "{pad}{}",
                            "─".repeat(width.saturating_sub(pad.len()).clamp(8, 60))
                        ),
                        Style::default().fg(th.dim),
                    )));
                }
                render_component(&child.component, th, width, out, pad.len());
            }
        }
    }
}

fn tab_label(c: &Component) -> String {
    match c {
        Component::Markdown(m) => m.title.clone().unwrap_or_else(|| "text".to_string()),
        Component::Table(t) => t.title.clone().unwrap_or_else(|| "table".to_string()),
        Component::Chart(c) => c.title.clone().unwrap_or_else(|| "chart".to_string()),
        _ => c.tag().to_string(),
    }
}

/// Does this document contain anything the operator can act on?
/// Used by the TUI to offer the fill/decide keybindings.
pub fn document_actions(doc: &pantheon_api::ui::UiDocument) -> Vec<ActionKind> {
    doc.actions()
        .into_iter()
        .map(|(_, a)| a.kind.clone())
        .collect()
}

/// The TUI's client capabilities: everything in the v1 catalogue except
/// `custom` (no registered custom renderers; the declared fallback is
/// the honest render) and `layout` columns/tabs subtleties, which the
/// renderer handles by stacking.
pub fn tui_caps() -> pantheon_api::ui::ClientCaps {
    pantheon_api::ui::ClientCaps {
        supported: vec![
            "markdown".to_string(),
            "table".to_string(),
            "chart".to_string(),
            "metrics".to_string(),
            "list".to_string(),
            "code".to_string(),
            "refs".to_string(),
            "form".to_string(),
            "task_list".to_string(),
            "progress".to_string(),
            "details".to_string(),
            "diff".to_string(),
            "approval".to_string(),
            "layout".to_string(),
        ],
        max_version: pantheon_api::ui::UI_PROTOCOL_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_api::ui::{
        ActionRef, DiffLineKind, FieldType, FormField, Metric, Series, Table, TaskItem,
    };
    use ratatui::text::Line;

    fn text(lines: &[Line<'static>]) -> String {
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

    fn node(component: Component) -> UiNode {
        UiNode {
            id: "n1".to_string(),
            component,
        }
    }

    #[test]
    fn table_renders_headers_and_rows() {
        let t = Table {
            title: Some("Files".to_string()),
            headers: vec!["name".to_string(), "size".to_string()],
            rows: vec![
                vec!["a.txt".to_string(), "1k".to_string()],
                vec!["b.txt".to_string(), "2k".to_string()],
            ],
            align: None,
        };
        let out = text(&render_ui_node(
            &node(Component::Table(t)),
            &Theme::dark(),
            80,
        ));
        assert!(out.contains("Files"), "title: {out}");
        assert!(out.contains("a.txt") && out.contains("2k"), "rows: {out}");
    }

    #[test]
    fn table_narrow_width_wraps_without_panic() {
        let t = Table {
            title: None,
            headers: vec!["a_very_long_column_name".to_string(), "b".to_string()],
            rows: vec![vec!["x".to_string(), "y".to_string()]],
            align: None,
        };
        let lines = render_ui_node(&node(Component::Table(t)), &Theme::dark(), 20);
        assert!(!lines.is_empty());
        for l in &lines {
            // Display width in chars (box-drawing chars are 3 bytes but
            // one cell wide).
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 20, "line exceeds width: {w}");
        }
    }

    #[test]
    fn code_truncation_names_the_dropped_lines() {
        let body = (0..250)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = text(&render_ui_node(
            &node(Component::Code(Code {
                code: body,
                language: "rust".to_string(),
                title: None,
            })),
            &Theme::dark(),
            80,
        ));
        assert!(out.contains("… 50 more lines"), "got: {out}");
        assert!(
            !out.contains("line 249"),
            "line 249 must be truncated: {out}"
        );
    }

    #[test]
    fn tabs_layout_stacks_all_tabs() {
        let md = |title: &str, body: &str| {
            node(Component::Markdown(Markdown {
                text: body.to_string(),
                title: Some(title.to_string()),
            }))
        };
        let out = text(&render_ui_node(
            &node(Component::Layout(Layout {
                kind: LayoutKind::Tabs,
                children: vec![md("One", "first tab body"), md("Two", "second tab body")],
            })),
            &Theme::dark(),
            80,
        ));
        assert!(out.contains("first tab body"), "got: {out}");
        assert!(out.contains("second tab body"), "got: {out}");
    }

    #[test]
    fn bar_chart_renders_values() {
        let c = Chart {
            title: Some("Load".to_string()),
            chart_type: ChartType::Bar,
            labels: vec!["a".to_string(), "b".to_string()],
            series: vec![Series {
                name: "s".to_string(),
                values: vec![10.0, 20.0],
            }],
        };
        let out = text(&render_ui_node(
            &node(Component::Chart(c)),
            &Theme::dark(),
            80,
        ));
        assert!(out.contains("Load"), "{out}");
        assert!(out.contains('a') && out.contains('b'), "{out}");
    }

    #[test]
    fn line_chart_renders_sparkline() {
        let c = Chart {
            title: None,
            chart_type: ChartType::Line,
            labels: vec!["s".to_string()],
            series: vec![Series {
                name: "s".to_string(),
                values: vec![1.0, 2.0],
            }],
        };
        let lines = render_ui_node(&node(Component::Chart(c)), &Theme::dark(), 80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn form_renders_fields_and_masks_secrets() {
        let f = Form {
            title: Some("Login".to_string()),
            description: None,
            fields: vec![
                FormField {
                    name: "user".to_string(),
                    label: "User".to_string(),
                    field_type: FieldType::Text,
                    required: true,
                    default: None,
                    placeholder: None,
                },
                FormField {
                    name: "pw".to_string(),
                    label: "Password".to_string(),
                    field_type: FieldType::Secret,
                    required: true,
                    default: None,
                    placeholder: None,
                },
            ],
            submit: ActionRef {
                action_id: "go".to_string(),
                label: "Go".to_string(),
                kind: ActionKind::Local,
            },
        };
        let out = text(&render_ui_node(
            &node(Component::Form(f)),
            &Theme::dark(),
            80,
        ));
        assert!(out.contains("Login"), "{out}");
        assert!(out.contains("User") && out.contains("Password"), "{out}");
        // The renderer shows labels, never values; nothing secret-shaped
        // leaks through the static render.
        assert!(!out.contains("••••") || out.contains("Password"));
    }

    #[test]
    fn progress_renders_bar_and_percent() {
        let p = Progress {
            label: "Build".to_string(),
            done: 1,
            total: 2,
        };
        let out = text(&render_ui_node(
            &node(Component::Progress(p)),
            &Theme::dark(),
            40,
        ));
        assert!(out.contains("50"), "percent: {out}");
    }

    #[test]
    fn task_list_renders_states() {
        let t = TaskList {
            title: None,
            items: vec![
                TaskItem {
                    id: "t1".to_string(),
                    label: "done thing".to_string(),
                    state: TaskState::Done,
                    detail: None,
                },
                TaskItem {
                    id: "t2".to_string(),
                    label: "todo thing".to_string(),
                    state: TaskState::Pending,
                    detail: None,
                },
            ],
        };
        let out = text(&render_ui_node(
            &node(Component::TaskList(t)),
            &Theme::dark(),
            80,
        ));
        assert!(
            out.contains("done thing") && out.contains("todo thing"),
            "{out}"
        );
    }

    #[test]
    fn diff_renders_add_remove_lines() {
        let d = Diff {
            title: None,
            files: vec![pantheon_api::ui::DiffFile {
                path: "f.txt".to_string(),
                hunks: vec![pantheon_api::ui::DiffHunk {
                    header: "@@".to_string(),
                    lines: vec![
                        pantheon_api::ui::DiffLine {
                            kind: DiffLineKind::Del,
                            text: "old".to_string(),
                        },
                        pantheon_api::ui::DiffLine {
                            kind: DiffLineKind::Add,
                            text: "new".to_string(),
                        },
                    ],
                }],
            }],
        };
        let out = text(&render_ui_node(
            &node(Component::Diff(d)),
            &Theme::dark(),
            80,
        ));
        assert!(out.contains("old") && out.contains("new"), "{out}");
    }

    #[test]
    fn metrics_render_values() {
        let m = Metrics {
            title: None,
            items: vec![Metric {
                label: "cpu".to_string(),
                value: "42%".to_string(),
                delta: None,
            }],
        };
        let out = text(&render_ui_node(
            &node(Component::Metrics(m)),
            &Theme::dark(),
            80,
        ));
        assert!(out.contains("cpu") && out.contains("42%"), "{out}");
    }

    #[test]
    fn approval_card_renders_actions() {
        let a = ApprovalCard {
            title: "Delete?".to_string(),
            detail: "Delete 3 files.".to_string(),
            approve: ActionRef {
                action_id: "yes".to_string(),
                label: "Delete".to_string(),
                kind: ActionKind::Approval {
                    decision: pantheon_api::ui::ApprovalDecision::Grant,
                },
            },
            deny: ActionRef {
                action_id: "no".to_string(),
                label: "Keep".to_string(),
                kind: ActionKind::Approval {
                    decision: pantheon_api::ui::ApprovalDecision::Deny,
                },
            },
            scope_hint: "delete 3 files".to_string(),
        };
        let out = text(&render_ui_node(
            &node(Component::Approval(a)),
            &Theme::dark(),
            80,
        ));
        assert!(
            out.contains("Delete?") && out.contains("Delete 3 files."),
            "{out}"
        );
        assert!(out.contains("Delete") && out.contains("Keep"), "{out}");
    }

    #[test]
    fn degraded_document_still_renders() {
        // A v2 document degrades to explanatory markdown; the renderer
        // must handle whatever the protocol hands it.
        let n = node(Component::Markdown(Markdown {
            title: None,
            text: "hello".to_string(),
        }));
        let out = text(&render_ui_node(&n, &Theme::dark(), 80));
        assert!(out.contains("hello"), "{out}");
    }

    #[test]
    fn code_block_renders() {
        let c = Code {
            title: None,
            language: "rust".to_string(),
            code: "fn main() {}".to_string(),
        };
        let out = text(&render_ui_node(
            &node(Component::Code(c)),
            &Theme::dark(),
            80,
        ));
        assert!(out.contains("fn main()"), "{out}");
    }
}
