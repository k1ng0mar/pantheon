//! Alternate-screen model browser.
//!
//! A richer replacement for the scroll-only `pick` that `pantheon model`
//! previously used: filter tabs, a four-column table, and a details card.
//! It deliberately returns a row index plus a chosen action, so the
//! caller (and not this widget) decides what saving means. The browser
//! is intentionally a TTY-only, side-effect-free value producer.

use crate::model_catalog::ModelRow;
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAction {
    Use,
    SetDefault,
    Test,
}

#[derive(Debug, Clone)]
pub struct BrowserResult {
    pub index: usize,
    pub action: ModelAction,
}

pub fn browse(title: &str, rows: &[ModelRow]) -> Option<BrowserResult> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode},
        execute,
        terminal::{enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };
    if rows.is_empty() {
        return None;
    }
    enable_raw_mode().ok()?;
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            use crossterm::{terminal::disable_raw_mode, execute, cursor};
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), cursor::Show);
        }
    }
    let _guard = Guard;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide).ok()?;
    let leave = |out: &mut std::io::Stdout| {
        let _ = execute!(*out, LeaveAlternateScreen);
    };
    let tabs: [&str; 5] = ["All", "Coding", "Reasoning", "Free", "Vision"];
    let mut tab = 0usize;
    let mut filter = String::new();
    let mut selected = 0usize;
    let mut detail = false;

    fn tab_match(tab: usize, row: &ModelRow) -> bool {
        let id = row.id.to_lowercase();
        match tab {
            0 => true,
            1 => id.contains("coder") || id.contains("code") || id.contains("codex"),
            2 => {
                id.contains("reasoning")
                    || id.contains("thinking")
                    || id.starts_with("o1")
                    || id.starts_with("o3")
                    || id.starts_with("o4")
                    || id.contains("-r1")
                    || id.contains(":r1")
            }
            3 => {
                matches!(row.input_per_mtok_usd, Some(0.0))
                    && matches!(row.output_per_mtok_usd, Some(0.0))
            }
            4 => {
                id.contains("vision")
                    || id.contains("4o")
                    || id.contains("claude")
                    || id.contains("gemini")
            }
            _ => true,
        }
    }

    loop {
        let mut hits: Vec<usize> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            if !tab_match(tab, r) {
                continue;
            }
            let f = filter.to_lowercase();
            if f.is_empty()
                || r.id.to_lowercase().contains(&f)
                || crate::model_catalog::row_meta(r)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&f)
            {
                hits.push(i);
            }
        }
        if hits.is_empty() {
            selected = 0;
        } else if selected >= hits.len() {
            selected = hits.len().saturating_sub(1);
        }

        execute!(stdout, cursor::MoveTo(0, 0)).ok();
        let _ = crossterm::terminal::Clear(crossterm::terminal::ClearType::All);
        println!("{title}\r");

        print!("  ");
        for (i, t) in tabs.iter().enumerate() {
            if i == tab {
                print!("\x1b[1;35m[{t}]\x1b[0m  ");
            } else {
                print!("[{t}]  ");
            }
        }
        println!("filter: {filter}\r");

        if !detail {
            println!("  Name                                 Context     Input      Output\\r");
            if hits.is_empty() {
                println!("  (no matches)\\r");
            }
            for (row_idx, &idx) in hits.iter().enumerate().take(14) {
                let r = &rows[idx];
                let marker = if row_idx == selected { ">" } else { " " };
                let ctx = r
                    .context_limit
                    .map(|c| format!("{}k", c / 1000))
                    .unwrap_or_else(|| "-".into());
                let inp = r
                    .input_per_mtok_usd
                    .map(|v| format!("${v:.2}"))
                    .unwrap_or_else(|| "-".into());
                let out = r
                    .output_per_mtok_usd
                    .map(|v| format!("${v:.2}"))
                    .unwrap_or_else(|| "-".into());
                let label = crate::sanitize::strip_control(&r.id);
                let max = 36usize;
                let label = if label.len() > max {
                    format!("{}...", &label[..max.saturating_sub(3)])
                } else {
                    format!("{label:<max$}")
                };
                if row_idx == selected {
                    println!(
                        "  {marker} \x1b[1;35m{label}\x1b[0m {ctx:>9}  {inp:>9}  {out:>9}\r"
                    );
                } else {
                    println!("  {marker} {label} {ctx:>9}  {inp:>9}  {out:>9}\r");
                }
            }
            println!(
                "\r  {} match{} · Enter details · Tab/1-5 filter · type search · Esc cancel\r",
                hits.len(),
                if hits.len() == 1 { "" } else { "es" }
            );
        } else if !hits.is_empty() {
            let r = &rows[hits[selected]];
            println!();
            println!("  MODEL DETAILS\r");
            println!("  Name      {}\r", r.id);
            println!(
                "  Context   {}\r",
                r.context_limit
                    .map(|c| format!("{c} tokens"))
                    .unwrap_or_else(|| "unknown".into())
            );
            println!(
                "  Input     {}\r",
                r.input_per_mtok_usd
                    .map(|v| format!("${v:.2} / 1M"))
                    .unwrap_or_else(|| "unknown".into())
            );
            println!(
                "  Output    {}\r",
                r.output_per_mtok_usd
                    .map(|v| format!("${v:.2} / 1M"))
                    .unwrap_or_else(|| "unknown".into())
            );
            println!(
                "  Source    {}\r",
                if r.curated { "curated + live" } else { "live only" }
            );
            println!();
            println!("  [Enter] use this model   [s] set as default   [t] test   [Esc] back\r");
        }
        let _ = stdout.flush();

        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => {
                leave(&mut stdout);
                return None;
            }
        };
        if let Event::Key(k) = ev {
            match k.code {
                KeyCode::Esc => {
                    if detail {
                        detail = false;
                    } else {
                        leave(&mut stdout);
                        return None;
                    }
                }
                KeyCode::Enter => {
                    if detail {
                        leave(&mut stdout);
                        return Some(BrowserResult {
                            index: hits[selected],
                            action: ModelAction::Use,
                        });
                    }
                    if !hits.is_empty() {
                        detail = true;
                    }
                }
                KeyCode::Char('s') if detail => {
                    leave(&mut stdout);
                    return Some(BrowserResult {
                        index: hits[selected],
                        action: ModelAction::SetDefault,
                    });
                }
                KeyCode::Char('t') if detail => {
                    leave(&mut stdout);
                    return Some(BrowserResult {
                        index: hits[selected],
                        action: ModelAction::Test,
                    });
                }
                KeyCode::Tab | KeyCode::Right => {
                    tab = (tab + 1) % tabs.len();
                    selected = 0;
                    detail = false;
                }
                KeyCode::Left => {
                    tab = if tab == 0 { tabs.len() - 1 } else { tab - 1 };
                    selected = 0;
                    detail = false;
                }
                KeyCode::Char('1') => {
                    tab = 0;
                    selected = 0;
                    detail = false;
                }
                KeyCode::Char('2') => {
                    tab = 1;
                    selected = 0;
                    detail = false;
                }
                KeyCode::Char('3') => {
                    tab = 2;
                    selected = 0;
                    detail = false;
                }
                KeyCode::Char('4') => {
                    tab = 3;
                    selected = 0;
                    detail = false;
                }
                KeyCode::Char('5') => {
                    tab = 4;
                    selected = 0;
                    detail = false;
                }
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < hits.len() {
                        selected += 1;
                    }
                }
                KeyCode::Home => selected = 0,
                KeyCode::End => selected = hits.len().saturating_sub(1),
                KeyCode::Backspace => {
                    filter.pop();
                    selected = 0;
                }
                KeyCode::Char('c') if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                    leave(&mut stdout);
                    return None;
                }
                KeyCode::Char(c) => {
                    filter.push(c);
                    selected = 0;
                }
                _ => {}
            }
        }
    }
}
