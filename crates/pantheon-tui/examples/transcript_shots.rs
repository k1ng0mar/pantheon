//! Screenshot harness for the Hermes-inspired transcript blocks.
//!
//! Renders a demo screen (tab bar, transcript, footer) through
//! `TestBackend` and writes it as a PNG. Temporary visual-verification
//! tooling - not shipped.
//!
//! Usage: `cargo run -p pantheon-tui --example transcript_shots OUT.png`

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use pantheon_tui::session::theme::Theme;
use pantheon_tui::transcript::{
    activity_summary_line, background_jobs_line, burst_object, jump_to_latest_line,
    run_stats_segment, EditView, ShellView, ThoughtView, ToolStatus, ToolView,
};

const CELL_W: u32 = 9;
const CELL_H: u32 = 18;

fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (12, 12, 14),
        Color::DarkGray => (105, 105, 105),
        Color::Gray => (170, 170, 170),
        Color::White => (240, 240, 240),
        Color::Red => (255, 90, 90),
        Color::LightRed => (255, 140, 140),
        Color::Green => (110, 235, 130),
        Color::LightGreen => (150, 255, 170),
        Color::Yellow => (240, 200, 90),
        Color::LightYellow => (255, 225, 140),
        Color::Blue => (100, 150, 255),
        Color::LightBlue => (150, 190, 255),
        Color::Magenta => (215, 130, 255),
        Color::LightMagenta => (230, 170, 255),
        Color::Cyan => (110, 220, 240),
        Color::LightCyan => (160, 235, 250),
        _ => (200, 200, 200),
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let mut mac = Vec::with_capacity(4 + data.len());
    mac.extend_from_slice(ty);
    mac.extend_from_slice(data);
    out.extend_from_slice(&crc32(&mac).to_be_bytes());
}

/// Minimal truecolor PNG writer: 8-bit RGB, one IDAT.
fn write_png(path: &Path, w: u32, h: u32, px: &[u8]) {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolor
    chunk(&mut out, b"IHDR", &ihdr);

    let mut raw = Vec::with_capacity((1 + w as usize * 3) * h as usize);
    for y in 0..h as usize {
        raw.push(0); // filter: none
        raw.extend_from_slice(&px[y * w as usize * 3..(y + 1) * w as usize * 3]);
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    let idat = enc.finish().unwrap();
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out).unwrap();
}

fn dim(th: &Theme, s: &str) -> Span<'static> {
    Span::styled(s.to_string(), Style::default().fg(th.dim))
}

fn demo_lines(th: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![
        // Tab bar mock.
        Line::from(vec![
            Span::styled(
                " pantheon ",
                Style::default()
                    .fg(th.tab_active)
                    .bg(th.tab_active_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  team-chat ", Style::default().fg(th.tab_idle)),
            dim(th, "                          Hermes-style transcript pass"),
        ]),
        Line::from(""),
        // User message.
        Line::from(Span::styled(
            "You",
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Add a retry helper with exponential backoff to the http client",
            Style::default().fg(th.body),
        )),
        Line::from(""),
    ];

    // Collapsed thought.
    let t1 = ThoughtView::collapsed(
        "The http client currently fails hard on 429s. I should add a retry helper with jittered backoff.",
        Some(Duration::from_millis(4000)),
    );
    lines.extend(t1.lines(th));
    lines.push(Line::from(""));

    // Activity burst summary.
    lines.push(activity_summary_line(1, burst_object("web_search"), th));
    lines.push(activity_summary_line(2, burst_object("read_file"), th));
    lines.push(Line::from(""));

    // Expanded thought.
    let mut t2 = ThoughtView::collapsed(
        "Retry on 429/503 only; cap at 5 attempts.\nJitter avoids thundering herds.",
        Some(Duration::from_millis(2300)),
    );
    t2.expanded = true;
    lines.extend(t2.lines(th));
    lines.push(Line::from(""));

    // Tool rows in three states.
    for (name, status, dur, toks) in [
        ("search.codebase", ToolStatus::Running, None, None),
        (
            "read_file",
            ToolStatus::Succeeded,
            Some(Duration::from_millis(400)),
            Some("2.1k".to_string()),
        ),
        (
            "read_file",
            ToolStatus::Failed,
            Some(Duration::from_millis(1200)),
            None,
        ),
    ] {
        let mut v = ToolView {
            display_name: name.to_string(),
            status,
            duration: dur,
            tokens: toks,
            expanded: false,
            args: vec![],
            detail: vec![],
        };
        if status == ToolStatus::Failed {
            v.expanded = true;
            v.args = vec!["path: src/missing.rs".to_string()];
            v.detail = vec!["no such file".to_string()];
        }
        lines.extend(v.lines(th));
    }
    lines.push(Line::from(""));

    // Shell card, collapsed, long output.
    let out: Vec<String> = (1..=20).map(|i| format!("src/file{i:02}.rs")).collect();
    let sh = ShellView {
        command: "find src -name '*.rs' | head -20".to_string(),
        output: out,
        status: ToolStatus::Succeeded,
        duration: Some(Duration::from_millis(800)),
        expanded: false,
    };
    lines.extend(sh.lines(th));
    lines.push(Line::from(""));

    // Edit diff card, collapsed.
    let old = "fn retry() {}\nfn backoff() {}\nfn jitter() {}\nfn cap() {}\nfn sleep() {}\nfn again() {}\nfn more() {}\nfn extra() {}\nfn last() {}\n";
    let new = "fn retry() {}\nfn backoff_ms() {}\nfn jitter() {}\nfn cap() {}\nfn sleep() {}\nfn again() {}\nfn more() {}\nfn extra() {}\nfn last() {}\n";
    let ev = EditView::from_texts("crates/pantheon-tui/src/http.rs", old, new, false);
    lines.extend(ev.lines(th));
    lines.push(Line::from(""));

    // Assistant message.
    lines.push(Line::from(Span::styled(
        "Pantheon",
        Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "Done - retry helper added with jittered exponential backoff.",
        Style::default().fg(th.body),
    )));
    lines.push(Line::from(""));

    // Jump-to-latest affordance (as if scrolled up).
    lines.push(jump_to_latest_line(th));
    lines.push(Line::from(""));

    // Footer mock.
    let mut footer = vec![
        dim(th, "Build · openai · gpt-4o "),
        dim(th, &run_stats_segment(Duration::from_secs(270), 7317)),
        dim(th, "   153.0k (76%)"),
    ];
    if let Some(bg) = background_jobs_line(2, th) {
        footer.push(Span::raw("   "));
        footer.extend(bg.spans);
    }
    footer.push(dim(th, "   ctrl+p commands"));
    lines.push(Line::from(footer));
    lines
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/transcript_shots.png".to_string());
    let th = Theme::pantheon();
    let backend = TestBackend::new(100, 52);
    let mut term = Terminal::new(backend).unwrap();
    let lines = demo_lines(&th);
    term.draw(|f| {
        let area = Rect::new(0, 0, 100, 52);
        f.render_widget(Paragraph::new(lines.clone()), area);
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let (bg_r, bg_g, bg_b) = rgb(th.bg);
    let (cw, ch) = (CELL_W as usize, CELL_H as usize);
    let (pw, ph) = (100 * cw, 52 * ch);
    let mut px = vec![0u8; pw * ph * 3];
    // Base fill.
    for p in px.chunks_exact_mut(3) {
        p[0] = bg_r;
        p[1] = bg_g;
        p[2] = bg_b;
    }
    for y in 0..52 {
        for x in 0..100 {
            let cell = &buf[(x as u16, y as u16)];
            let sym = cell.symbol();
            if sym.trim().is_empty() {
                continue;
            }
            let (fr, fg_, fb) = rgb(cell.fg);
            let bold = cell.modifier.contains(Modifier::BOLD);
            let (fr, fg_, fb) = if bold {
                (
                    fr.saturating_add(25),
                    fg_.saturating_add(25),
                    fb.saturating_add(25),
                )
            } else {
                (fr, fg_, fb)
            };
            // Glyph blob: inset rect suggesting a character.
            for dy in 4..ch - 4 {
                for dx in 1..cw - 1 {
                    let i = ((y * ch + dy) * pw + x * cw + dx) * 3;
                    px[i] = fr;
                    px[i + 1] = fg_;
                    px[i + 2] = fb;
                }
            }
            // CROSSED_OUT: strike line through the middle.
            if cell.modifier.contains(Modifier::CROSSED_OUT) {
                let dy = ch / 2;
                for dx in 0..cw {
                    let i = ((y * ch + dy) * pw + x * cw + dx) * 3;
                    px[i] = fr;
                    px[i + 1] = fg_;
                    px[i + 2] = fb;
                }
            }
        }
    }
    write_png(Path::new(&out), pw as u32, ph as u32, &px);
    println!("wrote {out}");
}
