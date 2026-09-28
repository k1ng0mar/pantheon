//! Session tab bar (opencode-style): one tab per open session across the
//! top of the TUI.
//!
//! This module owns the *model* and the *rendering* of the tab bar, plus the
//! pure keybinding map. It does not own the event loop or the session switch
//! itself — that stays in `session.rs`, which drives this module the way it
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
use pantheon_storage::RunListing;
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
}

impl Tab {
    /// Short human label: the title when present, else the run id stem.
    /// Mirrors the id-stem convention the /history and /sessions renderers
    /// use (`skip(4).take(8)`), so a tab and an overlay row agree.
    pub fn label(&self) -> String {
        match self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(t) => t.chars().take(MAX_LABEL_CHARS).collect(),
            None => self.run_id.chars().skip(4).take(8).collect(),
        }
    }
}

/// Longest label a tab shows before truncation.
pub const MAX_LABEL_CHARS: usize = 22;

/// An ordered list of session tabs with one active tab.
///
/// The tab list is rebuilt from `ledger_list_runs` whenever the driver
/// refreshes it; [`TabList::refresh_from_runs`] preserves the active tab by
/// run id across rebuilds so a title arriving mid-session does not steal
/// the selection.
#[derive(Debug, Default, Clone)]
pub struct TabList {
    tabs: Vec<Tab>,
    /// Index into `tabs` of the active tab. Always < len when non-empty.
    active: usize,
}

impl TabList {
    /// Build a tab list from `ledger_list_runs` output (newest first, the
    /// order the listing already carries).
    ///
    /// `busy` decides the per-tab busy indicator — the driver passes
    /// `|id| supervisor.has_active_lease(id).unwrap_or(false)`. It is a
    /// closure (not a supervisor reference) so this stays pure and testable.
    ///
    /// `active_run_id` is the currently selected session. It is included
    /// even when it holds no lease yet (a fresh `/new` has no ledger row),
    /// so the bar never loses the session the user is looking at.
    pub fn from_runs(
        runs: &[RunListing],
        busy: impl Fn(&str) -> bool,
        active_run_id: &str,
    ) -> Self {
        let mut list = TabList {
            tabs: Vec::new(),
            active: 0,
        };
        list.refresh_from_runs(runs, &busy, active_run_id);
        list
    }

    /// Rebuild from a fresh listing, keeping the active tab when its run id
    /// survives the refresh and falling back to the first tab otherwise.
    pub fn refresh_from_runs(
        &mut self,
        runs: &[RunListing],
        busy: impl Fn(&str) -> bool,
        active_run_id: &str,
    ) {
        let prev_active = self.active_run_id().map(str::to_string);
        self.tabs = runs
            .iter()
            .map(|(run_id, _status, _ts, title)| Tab {
                run_id: run_id.clone(),
                title: title.clone(),
                busy: busy(run_id),
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
                },
            );
        }
        self.active = 0;
        let target = active_run_id
            .is_empty()
            .then(|| prev_active)
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

    /// The run id the active tab selects — what the driver writes to
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
    /// Ctrl+Tab: next tab.
    Next,
    /// Ctrl+Shift+Tab (or Shift+Tab): previous tab.
    Prev,
    /// Alt+1..9: jump to tab N (1-based).
    Jump(usize),
}

/// Map a crossterm key event to a tab action. Pure — no terminal needed.
///
/// Notes on real terminals:
/// * Ctrl+Tab arrives as `Tab` + CONTROL on most terminals; some send it as
///   plain Tab or swallow it — those users still have Alt+1..9.
/// * Ctrl+Shift+Tab usually arrives as `BackTab` + CONTROL; plain Shift+Tab
///   arrives as `BackTab` + SHIFT and is mapped to Prev as a fallback.
/// * Alt+digit arrives as `Char` + ALT on most setups. A few terminals send
///   ESC followed by the digit instead; the driver can add an ESC-prefix
///   peek if it wants that path — this mapper only handles the ALT form.
pub fn tab_key_action(code: KeyCode, mods: KeyModifiers) -> Option<TabAction> {
    if mods.contains(KeyModifiers::CONTROL) {
        return match code {
            KeyCode::Tab => Some(TabAction::Next),
            KeyCode::BackTab => Some(TabAction::Prev),
            _ => None,
        };
    }
    if mods.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c @ '1'..='9') = code {
            return Some(TabAction::Jump((c as u8 - b'0') as usize));
        }
        return None;
    }
    if code == KeyCode::BackTab && mods == KeyModifiers::SHIFT {
        return Some(TabAction::Prev);
    }
    None
}

/// Separator glyph between tabs.
const TAB_SEP: &str = " │ ";
/// Busy glyph shown while the session has a running turn.
const BUSY_GLYPH: &str = "●";

/// Render the tab bar into `area` (expects a single row).
///
/// opencode-style: `[ 1 title ● │ 2 other ]` — the active tab is bold cyan,
/// inactive tabs dim, and a yellow ● marks sessions with a running turn.
/// When the bar is wider than the area, a window around the active tab is
/// shown so the selected tab is never the one clipped away.
pub fn render_tab_bar(
    f: &mut Frame,
    area: Rect,
    tabs: &TabList,
    theme: &crate::session::theme::Theme,
) {
    if area.width == 0 || area.height == 0 || tabs.is_empty() {
        return;
    }
    let width = area.width as usize;
    let active = tabs.active_index();

    // Per-tab segment text and display width.
    let segments: Vec<String> = tabs
        .tabs()
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let busy = if tab.busy {
                format!(" {BUSY_GLYPH}")
            } else {
                String::new()
            };
            format!(" {} {}{}", i + 1, tab.label(), busy)
        })
        .collect();
    let widths: Vec<usize> = segments.iter().map(|s| s.chars().count()).collect();

    // Window around the active tab: expand left, then right, while it fits.
    let sep_w = TAB_SEP.chars().count();
    let (mut start, mut end) = (active, active);
    let mut used = widths[active];
    while start > 0 && used + sep_w + widths[start - 1] <= width {
        start -= 1;
        used += sep_w + widths[start];
    }
    while end + 1 < segments.len() && used + sep_w + widths[end + 1] <= width {
        end += 1;
        used += sep_w + widths[end];
    }

    // Build the line: dim separators, bold active tab, dim inactive
    // tabs, and a yellow busy glyph as its own span so it reads as a
    // status rather than part of the name.
    let mut line_spans: Vec<Span> = Vec::new();
    for i in start..=end {
        if i > start {
            line_spans.push(Span::styled(TAB_SEP, Style::default().fg(theme.dim)));
        }
        let tab = &tabs.tabs()[i];
        let style = if i == active {
            Style::default()
                .fg(theme.tab_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.tab_idle)
        };
        line_spans.push(Span::styled(format!(" {} {}", i + 1, tab.label()), style));
        if tab.busy {
            line_spans.push(Span::styled(
                format!(" {BUSY_GLYPH}"),
                Style::default().fg(theme.running),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(line_spans)), area);
}
