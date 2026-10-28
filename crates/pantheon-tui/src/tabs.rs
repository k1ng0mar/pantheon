//! Session tab bar: one tab per open session across the
//! top of the TUI.
//!
//! This module owns the *model* and the *rendering* of the tab bar, plus the
//! pure keybinding map. It does not own the event loop or the session switch
//! itself - that stays in `session.rs`, which drives this module the way it
//! drives the /history overlay: build a [`TabList`] from
//! `supervisor.ledger_list_runs` (the existing /sessions machinery, no new
//! store), render it at the top of each frame, and route
//! [`tab_key_action`] results through the existing session-selection path
//! (`state.session_id = id` + `ledger_reopen_run` + `replay`).
//!
//! Rules the driver must keep:
//! * Only ONE session is interactive at a time. Switching tabs changes the
//!   *selected* session; turns keep running on their own threads in the
//!   background (plumbing owned elsewhere).
//! * Never force `state.ready = true` on a switch. The Enter guard in
//!   session.rs (`begin_turn`) is per-selection and must keep applying; a
//!   queued message or a running turn follows the selection at send time via
//!   `resolve_send_run_id`.
//! * The /sessions overlay stays the detailed view; this bar is the switcher.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// One open session in the tab bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    /// The run id this tab selects.
    pub run_id: String,
    /// Generated title, when the run has one.
    pub title: Option<String>,
    /// True when this session has a turn running somewhere (a lease on it).
    pub busy: bool,
    /// True when this session is parked on an approval (or a clarify
    /// question). Renders the amber dot - the "look at me" state.
    pub approval: bool,
}

impl Tab {
    /// Short human label: the title when present, else "new session".
    /// A run id stem (`17924076`) reads as a broken number on a fresh
    /// tab; the auto-titler replaces this with a real title after the
    /// first turn.
    pub fn label(&self) -> String {
        match self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(t) => t.chars().take(MAX_LABEL_CHARS).collect(),
            None => "new session".to_string(),
        }
    }
}

/// Longest label a tab shows before truncation.
pub const MAX_LABEL_CHARS: usize = 22;

/// An ordered list of session tabs with one active tab.
///
/// The tab list is rebuilt from the driver's explicit open-tab list
/// whenever it refreshes; [`TabList::refresh_from_explicit`] preserves
/// the active tab by run id across rebuilds so a title arriving
/// mid-session does not steal the selection.
#[derive(Debug, Default, Clone)]
pub struct TabList {
    tabs: Vec<Tab>,
    /// Index into `tabs` of the active tab. Always < len when non-empty.
    active: usize,
}

impl TabList {
    /// Rebuild from an explicit open-tab list: (run id, title) pairs in
    /// display order. This is the true open-tab model - the driver owns
    /// the list; the ledger is only consulted for titles, never for
    /// membership.
    pub fn refresh_from_explicit(
        &mut self,
        runs: &[(String, Option<String>)],
        busy: impl Fn(&str) -> bool,
        active_run_id: &str,
    ) {
        let prev_active = self.active_run_id().map(str::to_string);
        self.tabs = runs
            .iter()
            .map(|(run_id, title)| Tab {
                run_id: run_id.clone(),
                title: title.clone(),
                busy: busy(run_id),
                approval: false,
            })
            .collect();
        // The selected session may hold no lease yet (fresh /new) and thus
        // be absent from the live listing; pin it so the bar always shows
        // where the user is.
        if !active_run_id.is_empty() && !self.tabs.iter().any(|t| t.run_id == active_run_id) {
            self.tabs.insert(
                0,
                Tab {
                    run_id: active_run_id.to_string(),
                    title: None,
                    busy: false,
                    approval: false,
                },
            );
        }
        self.active = 0;
        let target = active_run_id
            .is_empty()
            .then_some(prev_active)
            .flatten()
            .unwrap_or_else(|| active_run_id.to_string());
        if !target.is_empty() {
            self.set_active_run_id(&target);
        }
    }

    /// Update the busy indicator for one run without rebuilding.
    pub fn set_busy(&mut self, run_id: &str, busy: bool) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.run_id == run_id) {
            tab.busy = busy;
        }
    }

    /// Update the approval indicator for one run without rebuilding.
    /// Set a tab's title in place (SessionTitled), without a full
    /// rebuild. No-op when the run has no tab.
    pub fn set_title(&mut self, run_id: &str, title: &str) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.run_id == run_id) {
            tab.title = Some(title.to_string());
        }
    }

    pub fn set_approval(&mut self, run_id: &str, approval: bool) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.run_id == run_id) {
            tab.approval = approval;
        }
    }

    /// Remove the tab for `run_id`. The run itself is untouched - runs are
    /// durable, so closing a tab parks the session; it stays reopenable
    /// from /sessions or the overview nav.
    pub fn remove(&mut self, run_id: &str) -> bool {
        let Some(pos) = self.tabs.iter().position(|t| t.run_id == run_id) else {
            return false;
        };
        self.tabs.remove(pos);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        } else if pos < self.active {
            self.active -= 1;
        }
        true
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn active_index(&self) -> usize {
        self.active.min(self.tabs.len().saturating_sub(1))
    }

    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active_index())
    }

    /// The run id the active tab selects - what the driver writes to
    /// `state.session_id` through the existing session-selection path.
    pub fn active_run_id(&self) -> Option<&str> {
        self.active().map(|t| t.run_id.as_str())
    }

    /// Cycle to the next tab, wrapping. Returns the newly active run id.
    pub fn cycle_next(&mut self) -> Option<&str> {
        if self.tabs.is_empty() {
            return None;
        }
        self.active = (self.active + 1) % self.tabs.len();
        self.active_run_id()
    }

    /// Cycle to the previous tab, wrapping.
    pub fn cycle_prev(&mut self) -> Option<&str> {
        if self.tabs.is_empty() {
            return None;
        }
        self.active = self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1);
        self.active_run_id()
    }

    /// Jump to tab `n` (1-based, as shown in the bar). Returns true when the
    /// selection changed; out-of-range numbers are ignored.
    pub fn jump(&mut self, n: usize) -> bool {
        if n == 0 || n > self.tabs.len() {
            return false;
        }
        let idx = n - 1;
        if idx == self.active {
            return false;
        }
        self.active = idx;
        true
    }

    /// Select the tab for `run_id`. Returns true when it exists.
    pub fn set_active_run_id(&mut self, run_id: &str) -> bool {
        match self.tabs.iter().position(|t| t.run_id == run_id) {
            Some(i) => {
                self.active = i;
                true
            }
            None => false,
        }
    }
}

/// What a tab keybinding asked for. The driver applies it through the
/// existing session-selection path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAction {
    /// Ctrl+Tab: next tab. (Shift+Tab also reaches here via the
    /// BackTab+SHIFT fallback in [`tab_key_action`].)
    Next,
    /// Ctrl+Shift+Tab: previous tab.
    Prev,
    /// Alt+1..9: jump to tab N (1-based).
    Jump(usize),
    /// Ctrl+T: open a new session tab (same as `/new`).
    NewTab,
    /// Ctrl+W: close the active tab. Parks the run - never kills it.
    CloseTab,
}

/// Map a crossterm key event to a tab action. Pure, no terminal needed.
///
/// Ctrl+Tab/BackTab cycle, Ctrl+T/W open/close, Alt+1..9 jump.
/// Plain `[`/`]` always type into the chat input (the caller gates them
/// out); Shift+BackTab falls back to Prev.
///
/// What terminals actually send:
/// - Ctrl+Tab usually arrives as `Tab` + CONTROL; some send plain Tab or
///   swallow it, and those users still have `[`/`]` and Alt+1..9.
/// - Ctrl+Shift+Tab usually arrives as `BackTab` + CONTROL; plain Shift+Tab
///   arrives as `BackTab` + SHIFT and is mapped to Prev as a fallback.
/// - Alt+digit arrives as `Char` + ALT on most setups. A few send ESC
///   followed by the digit instead; the driver can add an ESC-prefix peek
///   if it wants that path, and this mapper only handles the ALT form.
/// - `[`/`]` are also the image-preview cyclers, but the preview overlay
///   owns the keyboard while open, so there is no conflict.
pub fn tab_key_action(code: KeyCode, mods: KeyModifiers) -> Option<TabAction> {
    if mods.contains(KeyModifiers::CONTROL) {
        return match code {
            KeyCode::Tab => Some(TabAction::Next),
            KeyCode::BackTab => Some(TabAction::Prev),
            KeyCode::Char('t') | KeyCode::Char('T') => Some(TabAction::NewTab),
            KeyCode::Char('w') | KeyCode::Char('W') => Some(TabAction::CloseTab),
            _ => None,
        };
    }
    if mods.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c @ '1'..='9') = code {
            return Some(TabAction::Jump((c as u8 - b'0') as usize));
        }
        return None;
    }
    // BackTab+SHIFT deliberately maps to NOTHING here: it is the
    // permission-mode cycle key (handled by the main loop's BackTab
    // arm). It used to map to previous-tab, which made the cycle
    // unreachable on terminals that report Shift+Tab as BackTab+SHIFT
    // (most of them). Previous-tab remains via Ctrl+Shift+Tab
    // (Ctrl+BackTab, handled in the CONTROL branch above) and
    // Ctrl+Tab cycling.
    if mods.is_empty() {
        // `[` steps left (previous), `]` steps right (next) - the same
        // direction as the image-preview cyclers.
        match code {
            KeyCode::Char('[') => return Some(TabAction::Prev),
            KeyCode::Char(']') => return Some(TabAction::Next),
            _ => {}
        }
    }
    None
}

/// Separator glyph between tabs.
const TAB_SEP: &str = "  ";
/// Status dot: every tab carries one. Green = turn running, amber =
/// parked on approval/input (the look-at-me state), grey = idle.
const DOT_BUSY: &str = "●";
const DOT_APPROVAL: &str = "●";
const DOT_IDLE: &str = "○";

/// Click target in the tab bar, with its x-range relative to the
/// bar's left edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabHit {
    /// Switch to tab N (index into [`TabList::tabs`]).
    Switch(usize),
    /// Close tab N: the active tab's trailing `×`. Kept separate from
    /// [`TabHit::Switch`] so clicking `×` closes instead of switching
    /// (a no-op on the active tab, which is the only tab that shows it).
    Close(usize),
    /// The trailing `+`: open a new session tab.
    NewTab,
}

/// Render the tab bar into `area` (expects a single row).
///
/// Browser-style: the active tab sits on a subtle wash with bright bold
/// text and a dim `×`; inactive tabs are plain dim text. A colored dot
/// leads each tab - green for a running turn, amber for a parked
/// approval - and `+` at the end opens a new session. When the bar is
/// wider than the area, a window around the active tab is shown so the
/// selected tab is never the one clipped away.
///
/// Returns the clickable x-ranges (tabs and `+`) for the mouse handler.
/// An empty tab list still renders the `+` so the bar never vanishes.
pub fn render_tab_bar(
    f: &mut Frame,
    area: Rect,
    tabs: &TabList,
    theme: &crate::session::theme::Theme,
) -> Vec<(u16, u16, TabHit)> {
    let mut hits: Vec<(u16, u16, TabHit)> = Vec::new();
    if area.width == 0 || area.height == 0 {
        return hits;
    }
    if tabs.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " +",
                Style::default().fg(theme.dim),
            ))),
            area,
        );
        hits.push((0, 2, TabHit::NewTab));
        return hits;
    }
    let width = area.width as usize;
    let active = tabs.active_index();

    // Per-tab segment text and display width. No numbering: this is a
    // browser bar, not a list; Alt+1..9 still jumps by position. The
    // active tab carries a dim `×`; every tab pads to its wash.
    let widths: Vec<usize> = tabs
        .tabs()
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let label_w = tab.label().chars().count();
            // " ● " + label + " × " (active) or "  " (inactive).
            if i == active {
                label_w + 6
            } else {
                label_w + 5
            }
        })
        .collect();

    // Window around the active tab: expand left, then right, while it fits.
    // The trailing `+` always keeps its slot.
    let sep_w = TAB_SEP.chars().count();
    let plus_w = 2; // " +"
    let (mut start, mut end) = (active, active);
    let mut used = widths[active] + plus_w;
    while start > 0 && used + sep_w + widths[start - 1] <= width {
        start -= 1;
        used += sep_w + widths[start];
    }
    while end + 1 < widths.len() && used + sep_w + widths[end + 1] <= width {
        end += 1;
        used += sep_w + widths[end];
    }

    // Build the line: the active tab gets the wash + bright text, the
    // status dot keeps its own color so it reads as state rather than
    // part of the name. Amber is approval-only: the dot is the one place
    // outside the approval card that may use it.
    let mut line_spans: Vec<Span> = Vec::new();
    let mut x: u16 = 0;
    for (rel, tab) in tabs.tabs()[start..=end].iter().enumerate() {
        let i = start + rel;
        if rel > 0 {
            line_spans.push(Span::styled(TAB_SEP, Style::default().fg(theme.dim)));
            x += sep_w as u16;
        }
        let w = widths[i] as u16;
        if i == active {
            // Active segment: " ● " (3) + label + " × " (3). The ×
            // gets its own hit: inside the Switch range it could only
            // ever switch to the already-active tab.
            let label_w = tabs.tabs()[i].label().chars().count() as u16;
            hits.push((x, x + 3 + label_w, TabHit::Switch(i)));
            hits.push((x + 3 + label_w, x + w, TabHit::Close(i)));
        } else {
            hits.push((x, x + w, TabHit::Switch(i)));
        }
        x += w;
        let (dot, dot_color) = if tab.approval {
            (DOT_APPROVAL, theme.warning)
        } else if tab.busy {
            (DOT_BUSY, theme.success)
        } else {
            (DOT_IDLE, theme.dim)
        };
        if i == active {
            let wash = Style::default().bg(theme.tab_active_bg);
            line_spans.push(Span::styled(
                format!(" {dot} "),
                wash.fg(dot_color).add_modifier(Modifier::BOLD),
            ));
            line_spans.push(Span::styled(
                tab.label(),
                wash.fg(theme.tab_active).add_modifier(Modifier::BOLD),
            ));
            line_spans.push(Span::styled(" × ", wash.fg(theme.dim)));
        } else {
            line_spans.push(Span::styled(
                format!(" {dot} ",),
                Style::default().fg(dot_color),
            ));
            line_spans.push(Span::styled(
                tab.label(),
                Style::default().fg(theme.tab_idle),
            ));
            line_spans.push(Span::raw("  "));
        }
    }
    hits.push((x, x + 2, TabHit::NewTab));
    line_spans.push(Span::styled(" +", Style::default().fg(theme.dim)));
    f.render_widget(Paragraph::new(Line::from(line_spans)), area);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn tab(run_id: &str, title: Option<&str>) -> Tab {
        Tab {
            run_id: run_id.to_string(),
            title: title.map(str::to_string),
            busy: false,
            approval: false,
        }
    }

    #[test]
    fn shift_tab_is_not_previous_tab() {
        // Shift+Tab must reach the permission-mode cycle, not tab
        // switching: most terminals report it as BackTab+SHIFT.
        // Previous-tab stays on Ctrl+Shift+Tab (Ctrl+BackTab).
        assert_eq!(
            tab_key_action(KeyCode::BackTab, KeyModifiers::SHIFT),
            None
        );
        assert_eq!(
            tab_key_action(KeyCode::BackTab, KeyModifiers::CONTROL),
            Some(TabAction::Prev)
        );
        assert_eq!(tab_key_action(KeyCode::BackTab, KeyModifiers::NONE), None);
    }

    #[test]
    fn active_tab_close_glyph_has_its_own_hit() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut list = TabList::default();
        list.refresh_from_explicit(
            &[
                ("r1".to_string(), Some("Alpha".to_string())),
                ("r2".to_string(), Some("Beta".to_string())),
            ],
            |_| false,
            "r1",
        );
        let theme = crate::session::theme::Theme::pantheon();
        let backend = TestBackend::new(80, 1);
        let mut term = Terminal::new(backend).unwrap();
        let hits: std::sync::Mutex<Vec<(u16, u16, TabHit)>> = std::sync::Mutex::new(vec![]);
        term.draw(|f| {
            let h = render_tab_bar(f, f.area(), &list, &theme);
            *hits.lock().unwrap() = h;
        })
        .unwrap();
        let hits = hits.lock().unwrap();
        // Active "Alpha": " ○ " + "Alpha" + " × ". The × range must
        // close, not switch (switching to the active tab is a no-op).
        assert!(
            hits.contains(&(0, 8, TabHit::Switch(0))),
            "switch range wrong: {hits:?}"
        );
        assert!(
            hits.contains(&(8, 11, TabHit::Close(0))),
            "close range wrong: {hits:?}"
        );
    }

    #[test]
    fn active_tab_renders_brand_purple_inactive_unchanged() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut list = TabList::default();
        list.refresh_from_explicit(
            &[
                ("r1".to_string(), Some("Alpha".to_string())),
                ("r2".to_string(), Some("Beta".to_string())),
            ],
            |_| false,
            "r1",
        );
        let theme = crate::session::theme::Theme::pantheon();
        let backend = TestBackend::new(80, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            render_tab_bar(f, f.area(), &list, &theme);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let row: String = (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        // Scan cells, not string bytes: the dot/close glyphs are
        // multi-byte, so a byte index from str::find would land on the
        // wrong cell.
        let cells: Vec<String> = (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        let find_label = |label: &str| -> u16 {
            cells
                .windows(label.chars().count())
                .position(|w| w.concat() == label)
                .expect("label missing") as u16
        };
        let active_at = find_label("Alpha");
        let idle_at = find_label("Beta");
        assert_eq!(
            buf[(active_at, 0)].fg,
            ratatui::style::Color::Rgb(114, 35, 255),
            "active tab must be brand purple #7223FF, row: {row}"
        );
        assert_eq!(
            buf[(idle_at, 0)].fg, theme.tab_idle,
            "inactive tab must keep the idle colour, row: {row}"
        );
    }

    #[test]
    fn untitled_tab_shows_new_session_not_id_stem() {
        let t = tab("abcdefgh12345678", None);
        assert_eq!(t.label(), "new session");
        // Empty/blank titles count as untitled too.
        let t = tab("abcdefgh12345678", Some("   "));
        assert_eq!(t.label(), "new session");
    }

    #[test]
    fn titled_tab_shows_title_truncated() {
        let t = tab("abcdefgh12345678", Some("fix the login bug"));
        assert_eq!(t.label(), "fix the login bug");
        let long = "a".repeat(100);
        let t = tab("abcdefgh12345678", Some(&long));
        assert_eq!(t.label().chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn set_title_updates_matching_tab_only() {
        let mut list = TabList::default();
        list.refresh_from_explicit(
            &[
                ("r1".to_string(), None),
                ("r2".to_string(), Some("old".to_string())),
            ],
            |_| false,
            "r1",
        );
        list.set_title("r1", "fresh title");
        assert_eq!(list.tabs()[0].label(), "fresh title");
        assert_eq!(list.tabs()[1].label(), "old");
        // Unknown run id: no-op, no panic.
        list.set_title("nope", "x");
        assert_eq!(list.tabs().len(), 2);
    }

    #[test]
    fn ctrl_tab_maps_to_cycle_actions() {
        assert_eq!(
            tab_key_action(KeyCode::Tab, KeyModifiers::CONTROL),
            Some(TabAction::Next)
        );
        assert_eq!(
            tab_key_action(KeyCode::BackTab, KeyModifiers::CONTROL),
            Some(TabAction::Prev)
        );
        // Shift+Tab no longer falls back to Prev: it is the
        // permission-mode cycle key (most terminals report Shift+Tab
        // as BackTab+SHIFT, and the old mapping made the cycle
        // unreachable). Previous-tab stays on Ctrl+Shift+Tab, i.e.
        // Ctrl+BackTab above; Ctrl+Shift+Tab also carries SHIFT but
        // the CONTROL branch wins.
        assert_eq!(
            tab_key_action(KeyCode::BackTab, KeyModifiers::SHIFT),
            None
        );
        assert_eq!(
            tab_key_action(
                KeyCode::BackTab,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Some(TabAction::Prev)
        );
        assert_eq!(
            tab_key_action(KeyCode::Char('t'), KeyModifiers::CONTROL),
            Some(TabAction::NewTab)
        );
    }
}
