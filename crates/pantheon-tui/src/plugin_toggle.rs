//! Interactive plugin toggle screen: Hermes-style list with space/enter
//! to toggle.
//!
//! Lists every plugin (bundled + third-party) with kind, version,
//! enabled state, and the trust note - plugins run code with the
//! operator's user privileges, which is the distinction the approval UI
//! must surface (see `pantheon_extensions::bundled` docs): an MCP
//! server is an integration Pantheon talks to, never code it runs.
//!
//! The screen is a one-shot crossterm alternate-screen loop mirroring
//! `model::pick`: raw mode + a Drop guard that always restores the
//! terminal. Toggling goes through the same [`crate::plugins_verb`]
//! machinery as the `pantheon plugins enable/disable` verbs, so the
//! screen and the verbs can never disagree.

use crate::plugins_verb::{apply_toggle, collect_rows, PluginRow};
use std::path::Path;

/// The trust distinction, shown on every render. Condensed from the
/// `pantheon_extensions::bundled` module docs.
pub(crate) const TRUST_NOTE: &str =
    "trust: a plugin is code Pantheon runs on your machine with your user privileges - \
Pantheon does not sandbox plugins, so enabling one is consent to run its code. \
MCP servers are different: integrations Pantheon talks to, never code it runs.";

struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        use crossterm::terminal::disable_raw_mode;
        let _ = disable_raw_mode();
        use crossterm::{cursor, execute};
        let _ = execute!(std::io::stdout(), cursor::Show);
    }
}

/// One rendered row: `[*] name  kind  version  status  description`.
/// Pure so it is unit-testable without a TTY.
pub(crate) fn format_row(row: &PluginRow, selected: bool) -> String {
    let marker = if selected { ">" } else { " " };
    let check = if row.enabled { "[*]" } else { "[ ]" };
    let status = if row.enabled { "enabled" } else { "disabled" };
    let bundled = if row.bundled { " bundled" } else { "" };
    let line = format!(
        "{marker} {check} {:<24} {:<4} {:<10} {:<8} {}{}",
        row.name, row.kind, row.version, status, row.description, bundled
    );
    if selected {
        format!("\x1b[1m{line}\x1b[0m")
    } else {
        line
    }
}

/// Run the interactive toggle screen against `data_dir`. Falls back to
/// a plain list when the terminal cannot be taken over.
pub(crate) fn run_toggle_screen(data_dir: &Path) {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyModifiers},
        execute,
        terminal::{enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };

    let mut rows = collect_rows(data_dir);
    if rows.is_empty() {
        println!("no plugins installed");
        return;
    }
    if enable_raw_mode().is_err() {
        for r in &rows {
            println!("{}", format_row(r, false));
        }
        return;
    }
    let _guard = TermGuard;
    let mut stdout = std::io::stdout();
    if execute!(stdout, EnterAlternateScreen, cursor::Hide).is_err() {
        return;
    }
    // LeaveAlternateScreen must run before the guard's Drop (which only
    // restores raw mode + cursor); do it explicitly on every exit path.
    let leave = |out: &mut std::io::Stdout| {
        let _ = execute!(*out, LeaveAlternateScreen);
    };

    let mut selected = 0usize;
    let mut status = String::new();
    let visible_rows = 14usize;

    loop {
        if selected >= rows.len() {
            selected = rows.len().saturating_sub(1);
        }
        let start = if rows.len() <= visible_rows {
            0
        } else {
            selected
                .saturating_sub(visible_rows / 2)
                .min(rows.len() - visible_rows)
        };

        execute!(stdout, cursor::MoveTo(0, 0)).ok();
        let _ = crossterm::terminal::Clear(crossterm::terminal::ClearType::All);
        println!("Plugins - toggle what the agent may load\r");
        println!("  ↑↓ navigate · space/enter toggle · r refresh · q/Esc quit\r");
        println!("\r");
        for (i, row) in rows.iter().enumerate().skip(start).take(visible_rows) {
            println!("  {}\r", format_row(row, i == selected));
        }
        println!("\r");
        println!("  {TRUST_NOTE}\r");
        if !status.is_empty() {
            println!("\r  {status}\r");
        }
        let _ = std::io::Write::flush(&mut stdout);

        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => {
                leave(&mut stdout);
                return;
            }
        };
        if let Event::Key(k) = ev {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    leave(&mut stdout);
                    return;
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    leave(&mut stdout);
                    return;
                }
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < rows.len() {
                        selected += 1;
                    }
                }
                KeyCode::Home => selected = 0,
                KeyCode::End => selected = rows.len().saturating_sub(1),
                KeyCode::Char('r') => {
                    rows = collect_rows(data_dir);
                    status = format!("refreshed ({} plugins)", rows.len());
                }
                KeyCode::Char(' ') | KeyCode::Enter => {
                    if rows.is_empty() {
                        continue;
                    }
                    let row = &rows[selected];
                    let new_state = !row.enabled;
                    match apply_toggle(data_dir, row, new_state) {
                        Ok(msg) => {
                            rows[selected].enabled = new_state;
                            status = msg;
                        }
                        Err(e) => {
                            status = format!("error: {e}");
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins_verb::Toggle;

    fn row(name: &str, enabled: bool, bundled: bool) -> PluginRow {
        PluginRow {
            name: name.to_string(),
            kind: "tool",
            version: "1.0".to_string(),
            description: "Does things.".to_string(),
            bundled,
            enabled,
            toggle: Toggle::BundledRegistry,
        }
    }

    #[test]
    fn format_row_shows_state_and_bundled_tag() {
        let on = format_row(&row("demo", true, true), false);
        assert!(on.contains("[*]"), "enabled marker missing: {on}");
        assert!(on.contains("enabled"), "status missing: {on}");
        assert!(on.contains("bundled"), "bundled tag missing: {on}");
        assert!(on.contains("demo"), "name missing: {on}");
        assert!(!on.contains("\x1b[1m"), "unselected row must not be bold");

        let off = format_row(&row("demo", false, false), false);
        assert!(off.contains("[ ]"), "disabled marker missing: {off}");
        assert!(off.contains("disabled"), "status missing: {off}");
        assert!(
            !off.contains("bundled"),
            "third-party row wrongly tagged: {off}"
        );

        let sel = format_row(&row("demo", true, false), true);
        assert!(
            sel.starts_with(">\x1b[1m") || sel.contains("\x1b[1m"),
            "selected row must be bold: {sel:?}"
        );
        assert!(sel.contains('>'), "selected marker missing: {sel:?}");
    }

    #[test]
    fn trust_note_states_the_privilege_distinction() {
        assert!(
            TRUST_NOTE.contains("user privileges"),
            "trust note must name the privilege level"
        );
        assert!(
            TRUST_NOTE.contains("does not sandbox"),
            "trust note must state the no-sandbox fact"
        );
        assert!(
            TRUST_NOTE.contains("MCP"),
            "trust note must carry the plugin-vs-MCP distinction"
        );
    }
}
