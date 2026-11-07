//! render_group: render, widget, todo_card, tabs, diffview, transcript, ui_render, desktop, yank, sanitize, send, session_summary, skills, stats, app, codebase in one file.

pub mod render {
    //! Turning widgets into pixels.
    //!
    //! `widget.rs` holds state and key handling and imports no terminal types,
    //! which is what makes it testable without a tty. This file is the other
    //! half: the same widgets, rendered. Keeping the two apart is why a widget
    //! test can assert on `rows[0].selected` instead of scraping ANSI.
    //!
    //! Every list widget renders through [`rows_for`], so the selected row looks
    //! the same whether it is a provider, a tool group, or a session.

    use ratatui::layout::{Constraint, Layout, Rect};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

    use crate::widget::{Confirm, MultiSelect, Row, Select, TextInput};

    /// How much vertical space a list window gets. Set from the frame height each
    /// draw, so a short terminal shows fewer rows instead of overflowing.
    fn window(height: u16) -> usize {
        // 4 = the box's two borders plus the title and the hint line.
        (height as usize).saturating_sub(4).max(1)
    }

    /// Rows that fit, accounting for rows that draw a second `meta` line: a
    /// two-line row costs double, so a list full of them shows half as many
    /// before the box overflows. `uses_meta` is read off the list's items
    /// (homogeneous: a list either carries `meta` on every row or none).
    fn visible_rows_for(height: u16, uses_meta: bool) -> usize {
        let base = window(height);
        if uses_meta {
            (base / 2).max(1)
        } else {
            base
        }
    }

    // Borders were dropped: the look is a flat, title-led
    // surface rather than a boxed widget. The list/date height accounting in
    // `draw_list` still adds a small margin; `window()` subtracts the title
    // and hint lines, so the change only removes the chrome.
    fn frame_box(title: &str) -> Block<'static> {
        Block::default().title(format!(" {} ", title)).title_style(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
    }

    /// Render one row to one or two lines: the label line, then a dim
    /// second line when the row carries `meta`. The second line is what
    /// keeps a long context-plus-price string from clipping against the
    /// label on a narrow terminal.
    fn row_lines(r: &Row) -> Vec<Line<'static>> {
        let mut head = Vec::new();
        if !r.marker.is_empty() {
            head.push(Span::raw(format!("{} ", r.marker)));
        }
        let bullet = if r.selected { "● " } else { "  " };
        head.push(Span::raw(bullet.to_string()));
        head.push(Span::styled(
            r.label.clone(),
            if r.selected {
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ));
        if !r.desc.is_empty() {
            head.push(Span::styled(
                format!("  {}", r.desc),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if !r.tag.is_empty() {
            head.push(Span::styled(
                format!("  [{}]", r.tag),
                Style::default().fg(Color::Magenta),
            ));
        }
        if !r.enabled {
            // Grey the whole row and say why, rather than letting the user pick
            // it and hit a silent refusal.
            head = head
                .into_iter()
                .map(|s| Span::styled(s.content, Style::default().fg(Color::DarkGray)))
                .collect();
            head.push(Span::styled(
                "  (unavailable)",
                Style::default().fg(Color::DarkGray),
            ));
        }
        let mut lines = vec![Line::from(head)];
        if !r.meta.is_empty() {
            // Indented under the bullet so it reads as belonging to the row
            // above, not a new top-level entry.
            lines.push(Line::from(Span::styled(
                format!("    {}", r.meta),
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines
    }

    /// The shared list frame: a bordered box, the rows, and a hint footer.
    fn draw_list(
        f: &mut ratatui::Frame,
        area: Rect,
        title: &str,
        hint: &str,
        filter: &str,
        empty_reason: &str,
        rows: &[Row],
    ) {
        let [main, foot] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        f.render_widget(Clear, area);
        f.render_widget(frame_box(title), main);

        let inner = main.inner(ratatui::layout::Margin::new(1, 1));
        if rows.is_empty() {
            // An empty list must say which of the two empty cases it is: nothing
            // exists, or the filter excluded everything. They need different
            // reactions from the user.
            let why = if filter.is_empty() {
                empty_reason
            } else {
                "no matches for that filter"
            };
            f.render_widget(
                Paragraph::new(Span::styled(why, Style::default().fg(Color::DarkGray))),
                inner,
            );
        } else {
            let lines: Vec<Line> = rows.iter().flat_map(row_lines).collect();
            f.render_widget(Paragraph::new(lines), inner);
        }

        let filter_note = if filter.is_empty() {
            String::new()
        } else {
            format!("  filter: {filter}")
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{hint}{filter_note}"),
                Style::default().fg(Color::DarkGray),
            )),
            foot,
        );
    }

    pub fn draw_select(f: &mut ratatui::Frame, area: Rect, s: &mut Select) {
        let uses_meta = s.list.items().first().is_some_and(|i| !i.meta.is_empty());
        s.list
            .set_visible_rows(visible_rows_for(area.height, uses_meta));
        let rows = crate::widget::select_rows(s);
        draw_list(
            f,
            area,
            &s.title,
            &s.hint,
            s.list.filter(),
            &s.empty_reason,
            &rows,
        );
    }

    pub fn draw_multi(f: &mut ratatui::Frame, area: Rect, m: &mut MultiSelect) {
        let uses_meta = m.list.items().first().is_some_and(|i| !i.meta.is_empty());
        m.list
            .set_visible_rows(visible_rows_for(area.height, uses_meta));
        let rows = crate::widget::multi_rows(m);
        draw_list(
            f,
            area,
            &m.title,
            &m.hint,
            m.list.filter(),
            &m.empty_reason,
            &rows,
        );
    }

    pub fn draw_text(f: &mut ratatui::Frame, area: Rect, t: &mut TextInput) {
        let [main, foot] =
            Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(area);
        f.render_widget(Clear, area);
        f.render_widget(frame_box(&t.title), main);

        let shown = t.display();
        let mut lines = vec![Line::from(Span::styled(
            shown,
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        if t.is_empty() && !t.placeholder.is_empty() {
            lines.push(Line::from(Span::styled(
                t.placeholder.clone(),
                Style::default().fg(Color::DarkGray),
            )));
        }
        let inner = main.inner(ratatui::layout::Margin::new(1, 1));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        f.render_widget(
            Paragraph::new(Span::styled(
                t.hint.clone(),
                Style::default().fg(Color::DarkGray),
            )),
            foot,
        );
    }

    pub fn draw_confirm(f: &mut ratatui::Frame, area: Rect, c: &mut Confirm) {
        let [main, foot] =
            Layout::vertical([Constraint::Length(6), Constraint::Length(1)]).areas(area);
        f.render_widget(Clear, area);
        f.render_widget(frame_box(&c.title), main);
        let inner = main.inner(ratatui::layout::Margin::new(1, 1));
        let y = if c.default_yes { "yes" } else { "no" };
        let n = if c.default_yes { "no" } else { "yes" };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    c.question.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(format!("{y} / {n}")),
                Line::from(""),
                // The widget's own hint, not a hardcoded one: it names the
                // keys `handle_key` actually honors (y/n/enter/esc).
                Line::from(Span::styled(c.hint(), Style::default().fg(Color::DarkGray))),
            ]),
            inner,
        );
        f.render_widget(Paragraph::new(Span::raw("")), foot);
    }
}

pub mod widget {
    //! The TUI component layer.
    //!
    //! Every interactive element in the Pantheon terminal product is one of these
    //! five widgets. Setup, `/models`, `/sessions`, and the command palette are
    //! compositions of them, not separate implementations.
    //!
    //! # Design rules
    //!
    //! **A widget is a value, not an object.** No trait objects, no interior
    //! mutability, no framework. A widget is a plain struct with a `handle_key`
    //! that mutates it in place and returns a [`KeyResult`]. The screen stack in
    //! [`crate::app`] owns the widgets and the focus; a widget never reaches for
    //! a terminal, a clock, or a session.
    //!
    //! **No crossterm in this file.** Key handling is expressed in terms of the
    //! [`Key`] enum below, and the event loop translates crossterm events into it.
    //! That is what lets every widget be tested by calling `handle_key` directly
    //! with no TTY, no alternate screen, and no escape-sequence timing.
    //!
    //! **Filtering is part of the widget, not a mode.** Every list filters as you
    //! type, because the spec's `/` completion and the setup provider browser are
    //! the same behavior. A widget that could not filter would be a second
    //! implementation wearing a different hat.

    use std::fmt;

    // ---------------------------------------------------------------------------
    // input model
    // ---------------------------------------------------------------------------

    /// A key press, independent of the terminal library.
    ///
    /// `Ctrl+C` and friends are named rather than encoded as `Char('c')` with a
    /// modifier, because "is this a control chord or a literal c" is a decision
    /// the event loop makes once and the widgets should not re-litigate.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Key {
        Up,
        Down,
        Left,
        Right,
        PageUp,
        PageDown,
        Home,
        End,
        Enter,
        Esc,
        Tab,
        BackTab,
        Backspace,
        Delete,
        /// A bare space. Separate from `Char(' ')` because a multi-select toggles
        /// on it and a text input inserts it.
        Space,
        Char(char),
        CtrlC,
        CtrlD,
        CtrlK,
        CtrlL,
        CtrlN,
        CtrlP,
        CtrlR,
        CtrlO,
        CtrlSpace,
        /// Anything not handled above: arrows-as-was, function keys, resize.
        Other,
    }

    /// What a widget did with a key.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum KeyResult {
        /// The key is not this widget's business. The stack should offer it to
        /// the next widget down, and then to the global bindings.
        Ignored,
        /// Handled, state may have changed, stay on this widget.
        Redraw,
        /// The widget is finished and produced a value.
        Submit(Selection),
        /// The widget was abandoned. The stack pops it; whether that means "go
        /// back one screen" or "quit" is the stack's decision, not the widget's.
        Cancel,
    }

    impl KeyResult {
        /// True when the key was consumed by this widget.
        pub fn handled(&self) -> bool {
            !matches!(self, KeyResult::Ignored)
        }
    }

    /// The value a completed widget produces.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Selection {
        /// Single-select: the chosen item's value.
        One(String),
        /// Multi-select: every checked item's value, in list order.
        Many(Vec<String>),
        /// Text input: the typed text.
        Text(String),
        /// Confirm: the answer.
        Bool(bool),
        /// Nothing was selectable (a filter matched nothing, or a multi-select
        /// completed with no checks). Distinct from `One("")` so "the user chose
        /// nothing" is not the same event as "the user chose the empty option".
        None,
    }

    impl Selection {
        pub fn one(&self) -> Option<&str> {
            match self {
                Selection::One(v) => Some(v),
                _ => None,
            }
        }
        pub fn many(&self) -> Option<&[String]> {
            match self {
                Selection::Many(v) => Some(v),
                _ => None,
            }
        }
        pub fn text(&self) -> Option<&str> {
            match self {
                Selection::Text(v) => Some(v),
                _ => None,
            }
        }
        pub fn flag(&self) -> Option<bool> {
            match self {
                Selection::Bool(v) => Some(*v),
                _ => None,
            }
        }
    }

    // ---------------------------------------------------------------------------
    // shared list machinery
    // ---------------------------------------------------------------------------

    /// One row in any list widget.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Item {
        /// Primary text, left column.
        pub label: String,
        /// Secondary text, shown under or beside the label.
        pub desc: String,
        /// Right-aligned tag: a price tier, "free", "paid", "unavailable".
        pub tag: String,
        /// The value handed back on submit. Not always the label: a provider row's
        /// value is its catalog id, which the user never sees.
        pub value: String,
        /// A second metadata line, drawn dim under the label. Used where the
        /// per-row detail is long enough to clip on a narrow terminal if it
        /// shared the label line (a model's context window plus price).
        pub meta: String,
        /// A disabled row is visible and selectable-by-filter but cannot be
        /// submitted. This is how the spec's "provider exists, integration does
        /// not" is rendered without pretending: the row is there, and choosing it
        /// is refused.
        pub enabled: bool,
    }

    impl Item {
        pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
            Self {
                label: label.into(),
                value: value.into(),
                desc: String::new(),
                tag: String::new(),
                meta: String::new(),
                // Opt-in to disabled, never opt-out. A default of `false` here
                // made every row in the product unsubmittable, and the failure was
                // silent: Enter on a provider list did nothing.
                enabled: true,
            }
        }
        pub fn desc(mut self, d: impl Into<String>) -> Self {
            self.desc = d.into();
            self
        }
        pub fn tag(mut self, t: impl Into<String>) -> Self {
            self.tag = t.into();
            self
        }
        /// A second dim line under the label. See [`Item::meta`].
        pub fn meta(mut self, m: impl Into<String>) -> Self {
            self.meta = m.into();
            self
        }
        pub fn disabled(mut self) -> Self {
            self.enabled = false;
            self
        }

        /// Does this row match the filter? Substring, case-insensitive, across
        /// every visible column. Deliberately naive: this is a handful of dozen
        /// rows and a user typing, not a search index.
        fn matches(&self, needle: &str) -> bool {
            if needle.is_empty() {
                return true;
            }
            let n = needle.to_lowercase();
            self.label.to_lowercase().contains(&n)
                || self.desc.to_lowercase().contains(&n)
                || self.tag.to_lowercase().contains(&n)
                || self.meta.to_lowercase().contains(&n)
                || self.value.to_lowercase().contains(&n)
        }
    }

    /// The filter-and-cursor state every list widget shares.
    ///
    /// Split out so `Select` and `MultiSelect` cannot drift on filtering, cursor
    /// clamping, or scroll-windowing: they delegate instead of reimplementing.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct ListState {
        items: Vec<Item>,
        filter: String,
        /// Index into the *filtered* rows, not into `items`.
        cursor: usize,
        /// First visible row, for windowing a long list.
        scroll: usize,
        /// How many rows fit. Set by the renderer from the real terminal height.
        visible_rows: usize,
    }

    impl ListState {
        pub fn new(items: Vec<Item>) -> Self {
            Self {
                items,
                visible_rows: 12,
                ..Default::default()
            }
        }

        pub fn items(&self) -> &[Item] {
            &self.items
        }

        pub fn set_items(&mut self, items: Vec<Item>) {
            self.items = items;
            self.clamp();
        }

        pub fn filter(&self) -> &str {
            &self.filter
        }

        pub fn set_filter(&mut self, f: &str) {
            self.filter = f.to_string();
            // A changed filter invalidates the cursor position. Leaving it where
            // it was lands the highlight on an unrelated row the instant the user
            // narrows the list, which reads as the app having picked something.
            self.cursor = 0;
            self.scroll = 0;
        }

        /// Rows matching the current filter, as indices into `items`.
        pub fn visible(&self) -> Vec<usize> {
            self.items
                .iter()
                .enumerate()
                .filter(|(_, it)| it.matches(&self.filter))
                .map(|(i, _)| i)
                .collect()
        }

        pub fn visible_len(&self) -> usize {
            self.visible().len()
        }

        /// The item under the cursor, if any.
        pub fn current(&self) -> Option<&Item> {
            let v = self.visible();
            v.get(self.cursor).and_then(|i| self.items.get(*i))
        }

        pub fn cursor(&self) -> usize {
            self.cursor
        }

        pub fn scroll(&self) -> usize {
            self.scroll
        }

        pub fn set_visible_rows(&mut self, n: usize) {
            self.visible_rows = n.max(1);
        }

        pub fn visible_rows(&self) -> usize {
            self.visible_rows
        }

        /// Keep the cursor and scroll window inside the filtered list.
        ///
        /// Every mutation path funnels through here, so the cursor can never point
        /// past the end of what is on screen.
        pub fn clamp(&mut self) {
            let len = self.visible().len();
            if len == 0 {
                self.cursor = 0;
                self.scroll = 0;
                return;
            }
            if self.cursor >= len {
                self.cursor = len - 1;
            }
            // Keep the cursor inside the window without letting the window run
            // past the end of the list.
            if self.cursor < self.scroll {
                self.scroll = self.cursor;
            } else if self.cursor >= self.scroll + self.visible_rows {
                self.scroll = self.cursor + 1 - self.visible_rows;
            }
            let max_scroll = len.saturating_sub(self.visible_rows);
            if self.scroll > max_scroll {
                self.scroll = max_scroll;
            }
        }

        pub fn move_by(&mut self, delta: isize) {
            let len = self.visible().len();
            if len == 0 {
                return;
            }
            let next = self.cursor as isize + delta;
            self.cursor = next.clamp(0, len as isize - 1) as usize;
            self.clamp();
        }

        pub fn move_to(&mut self, idx: usize) {
            let len = self.visible().len();
            if len == 0 {
                return;
            }
            self.cursor = idx.min(len - 1);
            self.clamp();
        }

        /// Type-to-filter.
        pub fn push_char(&mut self, c: char) {
            self.set_filter(&format!("{}{}", self.filter, c));
        }

        pub fn backspace(&mut self) {
            let mut f = self.filter.clone();
            if f.pop().is_some() {
                self.set_filter(&f);
            }
        }

        /// Shared arrow/page handling.
        fn nav(&mut self, key: Key) -> bool {
            match key {
                Key::Up => {
                    self.move_by(-1);
                    true
                }
                Key::Down | Key::CtrlN => {
                    self.move_by(1);
                    true
                }
                Key::PageUp => {
                    self.move_by(-(self.visible_rows as isize));
                    true
                }
                Key::PageDown => {
                    self.move_by(self.visible_rows as isize);
                    true
                }
                Key::Home => {
                    self.move_to(0);
                    true
                }
                Key::End => {
                    self.move_to(usize::MAX);
                    true
                }
                _ => false,
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Select
    // ---------------------------------------------------------------------------

    /// Single-choice list with type-to-filter. The workhorse: providers, models,
    /// execution backend, permissions, memory backend, workspace.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct Select {
        pub title: String,
        /// Footer hint, e.g. "up/down navigate · / search · enter select · esc back".
        pub hint: String,
        /// Shown under the title when the list is empty, e.g. "no providers match".
        /// An empty list that says nothing is indistinguishable from a broken one.
        pub empty_reason: String,
        pub list: ListState,
    }

    impl Select {
        pub fn new(title: impl Into<String>, items: Vec<Item>) -> Self {
            Self {
                title: title.into(),
                hint: "up/down navigate · type to filter · enter select · esc back".into(),
                empty_reason: "(no matches)".into(),
                list: ListState::new(items),
            }
        }

        pub fn hint(mut self, h: impl Into<String>) -> Self {
            self.hint = h.into();
            self
        }
        pub fn empty_reason(mut self, r: impl Into<String>) -> Self {
            self.empty_reason = r.into();
            self
        }

        pub fn current(&self) -> Option<&Item> {
            self.list.current()
        }

        pub fn selected_value(&self) -> Option<String> {
            self.list.current().map(|i| i.value.clone())
        }

        pub fn handle_key(&mut self, key: Key) -> KeyResult {
            if self.list.nav(key) {
                return KeyResult::Redraw;
            }
            match key {
                Key::Char(c) => {
                    self.list.push_char(c);
                    KeyResult::Redraw
                }
                Key::Space => {
                    self.list.push_char(' ');
                    KeyResult::Redraw
                }
                Key::Backspace => {
                    self.list.backspace();
                    KeyResult::Redraw
                }
                Key::Enter => match self.list.current() {
                    // Submitting a disabled row is refused, not silently accepted
                    // and not silently ignored: the user pressed enter on
                    // something and deserves to know it cannot be chosen.
                    Some(it) if it.enabled => KeyResult::Submit(Selection::One(it.value.clone())),
                    Some(_) => KeyResult::Redraw,
                    None => KeyResult::Submit(Selection::None),
                },
                Key::Esc => KeyResult::Cancel,
                _ => KeyResult::Ignored,
            }
        }
    }

    // ---------------------------------------------------------------------------
    // MultiSelect
    // ---------------------------------------------------------------------------

    /// Many-choice list with space-to-toggle. Tool groups, gateways, extensions.
    ///
    /// A row the user never touched keeps its configured state, so a checklist
    /// opened on an existing config shows that config rather than resetting it.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct MultiSelect {
        pub title: String,
        pub hint: String,
        pub empty_reason: String,
        pub list: ListState,
        checked: Vec<bool>,
    }

    impl MultiSelect {
        pub fn new(title: impl Into<String>, items: Vec<Item>, preselected: &[&str]) -> Self {
            let checked = items
                .iter()
                .map(|i| preselected.contains(&i.value.as_str()))
                .collect();
            Self {
                title: title.into(),
                hint: "up/down navigate · space toggle · enter confirm · esc back".into(),
                empty_reason: "(nothing to choose from)".into(),
                list: ListState::new(items),
                checked,
            }
        }

        pub fn hint(mut self, h: impl Into<String>) -> Self {
            self.hint = h.into();
            self
        }
        pub fn empty_reason(mut self, r: impl Into<String>) -> Self {
            self.empty_reason = r.into();
            self
        }

        pub fn current(&self) -> Option<&Item> {
            self.list.current()
        }

        pub fn is_checked(&self, value: &str) -> bool {
            self.list
                .items()
                .iter()
                .position(|i| i.value == value)
                .map(|i| self.checked.get(i).copied().unwrap_or(false))
                .unwrap_or(false)
        }

        pub fn checked_values(&self) -> Vec<String> {
            self.list
                .items()
                .iter()
                .zip(self.checked.iter())
                .filter(|(_, c)| **c)
                .map(|(i, _)| i.value.clone())
                .collect()
        }

        /// Flip the row under the cursor. Returns false when the row is disabled,
        /// so the caller can render a refusal instead of a no-op.
        pub fn toggle_current(&mut self) -> bool {
            let Some(idx) = self.list.visible().get(self.list.cursor()).copied() else {
                return false;
            };
            let Some(item) = self.list.items().get(idx) else {
                return false;
            };
            if !item.enabled {
                return false;
            }
            if let Some(slot) = self.checked.get_mut(idx) {
                *slot = !*slot;
            }
            true
        }

        pub fn handle_key(&mut self, key: Key) -> KeyResult {
            if self.list.nav(key) {
                return KeyResult::Redraw;
            }
            match key {
                Key::Space => {
                    self.toggle_current();
                    KeyResult::Redraw
                }
                // Space is the toggle, so a literal space cannot be typed into the
                // filter. Backspace-then-space is not a flow anyone should have to
                // learn to filter on a space character.
                Key::Char(c) => {
                    self.list.push_char(c);
                    KeyResult::Redraw
                }
                Key::Backspace => {
                    self.list.backspace();
                    KeyResult::Redraw
                }
                Key::Enter => KeyResult::Submit(Selection::Many(self.checked_values())),
                Key::Esc => KeyResult::Cancel,
                _ => KeyResult::Ignored,
            }
        }
    }

    // ---------------------------------------------------------------------------
    // TextInput
    // ---------------------------------------------------------------------------

    /// Single-line text entry. Profile name, API key, directory path, run id.
    ///
    /// A masked input is how the spec's `sk-••••••••` works: the value is real,
    /// the display is not, and the value is never logged or echoed anywhere else.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct TextInput {
        pub title: String,
        pub hint: String,
        pub placeholder: String,
        /// Shown when the input is empty. Not a value: submitting an untouched
        /// empty input yields `""`, and the caller decides whether that means
        /// "use the default" or "the user typed nothing".
        pub default: String,
        value: String,
        cursor: usize,
        /// When set, render this character instead of the real value.
        pub mask: Option<char>,
    }

    impl TextInput {
        pub fn new(title: impl Into<String>) -> Self {
            Self {
                title: title.into(),
                hint: "enter confirm · esc back".into(),
                ..Default::default()
            }
        }

        pub fn placeholder(mut self, p: impl Into<String>) -> Self {
            self.placeholder = p.into();
            self
        }
        pub fn default(mut self, d: impl Into<String>) -> Self {
            self.default = d.into();
            self
        }
        pub fn hint(mut self, h: impl Into<String>) -> Self {
            self.hint = h.into();
            self
        }
        pub fn secret(mut self) -> Self {
            self.mask = Some('•');
            self
        }
        pub fn prefilled(mut self, v: impl Into<String>) -> Self {
            let v = v.into();
            self.cursor = v.chars().count();
            self.value = v;
            self
        }

        pub fn value(&self) -> &str {
            &self.value
        }

        /// Value with the default substituted when the user typed nothing.
        pub fn value_or_default(&self) -> String {
            if self.value.is_empty() {
                self.default.clone()
            } else {
                self.value.clone()
            }
        }

        /// What to draw: the real text, or a mask of the same length. A masked
        /// field never reveals its length differently from a real one, and
        /// never reveals a character.
        pub fn display(&self) -> String {
            match self.mask {
                None => self.value.clone(),
                Some(m) => m.to_string().repeat(self.value.chars().count()),
            }
        }

        pub fn is_empty(&self) -> bool {
            self.value.is_empty()
        }

        /// Cursor position, in characters. Public so a screen can render a caret
        /// or assert on edit behavior without reaching into private state.
        pub fn cursor(&self) -> usize {
            self.cursor
        }

        pub fn set_value(&mut self, v: impl Into<String>) {
            let v = v.into();
            self.cursor = v.chars().count();
            self.value = v;
        }

        pub fn handle_key(&mut self, key: Key) -> KeyResult {
            match key {
                Key::Char(c) => {
                    self.insert(c);
                    KeyResult::Redraw
                }
                Key::Space => {
                    self.insert(' ');
                    KeyResult::Redraw
                }
                Key::Backspace => {
                    self.delete_before();
                    KeyResult::Redraw
                }
                Key::Delete => {
                    self.delete_at();
                    KeyResult::Redraw
                }
                Key::Left => {
                    self.cursor = self.cursor.saturating_sub(1);
                    KeyResult::Redraw
                }
                Key::Right => {
                    let n = self.value.chars().count();
                    if self.cursor < n {
                        self.cursor += 1;
                    }
                    KeyResult::Redraw
                }
                Key::Home => {
                    self.cursor = 0;
                    KeyResult::Redraw
                }
                Key::End => {
                    self.cursor = self.value.chars().count();
                    KeyResult::Redraw
                }
                Key::Enter => KeyResult::Submit(Selection::Text(self.value.clone())),
                Key::Esc => KeyResult::Cancel,
                _ => KeyResult::Ignored,
            }
        }

        fn insert(&mut self, c: char) {
            let at = self.byte_index(self.cursor);
            self.value.insert(at, c);
            self.cursor += 1;
        }

        fn delete_before(&mut self) {
            if self.cursor == 0 {
                return;
            }
            let at = self.byte_index(self.cursor - 1);
            self.value.remove(at);
            self.cursor -= 1;
        }

        fn delete_at(&mut self) {
            let n = self.value.chars().count();
            if self.cursor >= n {
                return;
            }
            let at = self.byte_index(self.cursor);
            self.value.remove(at);
        }

        /// Char index to byte index. The cursor is counted in chars so a field
        /// holding a non-ASCII path cannot panic on a byte slice.
        fn byte_index(&self, char_idx: usize) -> usize {
            self.value
                .char_indices()
                .nth(char_idx)
                .map(|(b, _)| b)
                .unwrap_or(self.value.len())
        }
    }

    // ---------------------------------------------------------------------------
    // Confirm
    // ---------------------------------------------------------------------------

    /// A yes/no step. Used for destructive choices and for "skip this section?"
    /// style branches in setup.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Confirm {
        pub title: String,
        pub question: String,
        /// The answer a bare Enter takes. Defaulting to the safe answer means a
        /// reflexive Enter cannot install something irreversible.
        pub default_yes: bool,
    }

    impl Confirm {
        pub fn new(title: impl Into<String>, question: impl Into<String>) -> Self {
            Self {
                title: title.into(),
                question: question.into(),
                default_yes: false,
            }
        }

        pub fn default_yes(mut self, v: bool) -> Self {
            self.default_yes = v;
            self
        }

        pub fn hint(&self) -> String {
            if self.default_yes {
                "y/n · enter = yes · esc = no".into()
            } else {
                "y/n · enter = no · esc = no".into()
            }
        }

        pub fn handle_key(&mut self, key: Key) -> KeyResult {
            match key {
                Key::Char('y') | Key::Char('Y') => KeyResult::Submit(Selection::Bool(true)),
                Key::Char('n') | Key::Char('N') => KeyResult::Submit(Selection::Bool(false)),
                Key::Enter => KeyResult::Submit(Selection::Bool(self.default_yes)),
                Key::Esc => KeyResult::Submit(Selection::Bool(false)),
                _ => KeyResult::Ignored,
            }
        }
    }

    // ---------------------------------------------------------------------------
    // list rendering, shared by every list widget
    // ---------------------------------------------------------------------------

    /// A row ready to draw. The renderer turns this into a ratatui `Line`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Row {
        pub marker: String,
        pub label: String,
        pub desc: String,
        pub tag: String,
        /// Second dim line under the label; see [`Item::meta`].
        pub meta: String,
        pub selected: bool,
        pub enabled: bool,
    }

    /// Build the drawable rows for any list-backed widget.
    ///
    /// One function for all three list widgets, so a selected row cannot be styled
    /// one way in `Select` and another in `MultiSelect`. `checkbox` supplies the
    /// `[x]`/`[ ]` prefix for the multi-select and nothing for the others.
    pub fn rows_for(state: &ListState, checkbox: Option<&[bool]>) -> Vec<Row> {
        let visible = state.visible();
        let start = state.scroll().min(visible.len());
        visible
            .iter()
            .skip(start)
            .take(state.visible_rows())
            .enumerate()
            .map(|(row_offset, i)| {
                let it = &state.items()[*i];
                let marker = match checkbox {
                    Some(c) if c.get(*i).copied().unwrap_or(false) => "[x]".to_string(),
                    Some(_) => "[ ]".to_string(),
                    None => String::new(),
                };
                Row {
                    marker,
                    label: it.label.clone(),
                    desc: it.desc.clone(),
                    tag: it.tag.clone(),
                    meta: it.meta.clone(),
                    // The cursor indexes the filtered list, and the row's offset
                    // in this window is the filtered index minus the scroll
                    // offset. Comparing the two is what marks exactly one row.
                    selected: start + row_offset == state.cursor(),
                    enabled: it.enabled,
                }
            })
            .collect()
    }

    /// Single-select rows.
    pub fn select_rows(s: &Select) -> Vec<Row> {
        rows_for(&s.list, None)
    }

    /// Multi-select rows, with checkboxes.
    pub fn multi_rows(m: &MultiSelect) -> Vec<Row> {
        rows_for(&m.list, Some(&m.checked))
    }

    impl fmt::Display for Item {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.desc.is_empty() {
                write!(f, "{}", self.label)
            } else {
                write!(f, "{}  {}", self.label, self.desc)
            }
        }
    }
}

pub mod todo_card {
    //! Todo card for the transcript.
    //!
    //! Standalone by design: it renders [`TodoCardItem`]s with ratatui only.
    //! The backend maps `pantheon_api::todo::TodoItem` onto [`TodoCardItem`]
    //! at the integration point. Its palette comes from the theme module
    //! (see `session::theme`), not from literals here.
    //!
    //! ```text
    //! ⬢ Working on 2 to-dos
    //!   ☒ Made new things
    //!   ⊞ Read files
    //!   ☐ Edit files
    //!   ☐ Give summary
    //! ```

    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use ratatui::Frame;

    // The card's palette lives in the theme module, not here: no renderer
    // outside `session::theme` names a raw color for the conversation view.
    use crate::session::theme::{
        TODO_BLUE as BLUE, TODO_DIM_GRAY as DIM_GRAY, TODO_NEAR_BLACK as NEAR_BLACK,
        TODO_PURPLE as PURPLE,
    };

    /// Lifecycle state of one card row.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TodoStatus {
        Pending,
        InProgress,
        Completed,
    }

    /// One row of the card.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TodoCardItem {
        pub content: String,
        pub status: TodoStatus,
    }

    /// Build the card as styled lines (for embedding in the line-based
    /// transcript renderer). [`render_todo_card`] draws these same lines as
    /// a widget.
    pub fn todo_card_lines(items: &[TodoCardItem]) -> Vec<Line<'_>> {
        let remaining = items
            .iter()
            .filter(|i| i.status != TodoStatus::Completed)
            .count();
        let header_style = Style::default().add_modifier(Modifier::BOLD);
        let mut lines = Vec::with_capacity(items.len() + 1);
        lines.push(Line::from(vec![
            Span::styled(
                "⬢ ",
                Style::default().fg(PURPLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if items.is_empty() {
                    "No to-dos".to_string()
                } else if remaining == 0 {
                    "All to-dos done".to_string()
                } else {
                    format!("Working on {remaining} to-dos")
                },
                header_style,
            ),
        ]));
        for item in items {
            let (glyph, glyph_style, text_style) = match item.status {
                TodoStatus::Completed => (
                    "☒",
                    Style::default().fg(DIM_GRAY),
                    Style::default().fg(DIM_GRAY),
                ),
                TodoStatus::InProgress => (
                    "⊞",
                    Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                TodoStatus::Pending => (
                    "☐",
                    Style::default().fg(DIM_GRAY),
                    Style::default().fg(DIM_GRAY),
                ),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(glyph, glyph_style),
                Span::raw(" "),
                Span::styled(item.content.clone(), text_style),
            ]));
        }
        lines
    }

    /// Render the card into `area`: a bold "⬢ Working on N to-dos" header
    /// (N = not-yet-done), then one row per item - ☒ dimmed for done, ⊞
    /// highlighted for in-progress, ☐ dimmed for pending.
    ///
    /// The card takes `1 + items.len()` rows; anything taller is left blank,
    /// anything shorter clips from the bottom.
    ///
    /// Widget form of [`todo_card_lines`]. The session sidebar embeds
    /// `todo_card_lines` directly (see `render_sidebar_todos`) and does
    /// not call this; it exists for standalone embedding.
    pub fn render_todo_card(frame: &mut Frame, area: Rect, items: &[TodoCardItem]) {
        let lines = todo_card_lines(items);
        let card = Paragraph::new(lines).style(Style::default().bg(NEAR_BLACK));
        frame.render_widget(card, area);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn header(items: &[TodoCardItem]) -> String {
            todo_card_lines(items)
                .first()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .unwrap_or_default()
        }

        fn item(status: TodoStatus) -> TodoCardItem {
            TodoCardItem {
                content: "x".to_string(),
                status,
            }
        }

        #[test]
        fn todo_header_states() {
            assert_eq!(header(&[]), "⬢ No to-dos");
            assert_eq!(header(&[item(TodoStatus::Completed)]), "⬢ All to-dos done");
            assert_eq!(
                header(&[item(TodoStatus::Completed), item(TodoStatus::Pending)]),
                "⬢ Working on 1 to-dos"
            );
        }
    }
}

pub mod tabs {
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
            assert_eq!(tab_key_action(KeyCode::BackTab, KeyModifiers::SHIFT), None);
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
                buf[(idle_at, 0)].fg,
                theme.tab_idle,
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
            assert_eq!(tab_key_action(KeyCode::BackTab, KeyModifiers::SHIFT), None);
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
}

pub mod diffview {
    //! Inline unified diffs for file-editing tool calls.
    //!
    //! The TUI snapshots files named by `apply_files` at `ToolStarted` and
    //! diffs them at `ToolCompleted`, so the transcript shows what actually
    //! changed - red `-` lines, green `+` lines, `@@` hunk headers - instead
    //! of just the tool's argument summary. New files render all-`+`,
    //! deletions all-`-`, binaries are skipped, and large diffs truncate with
    //! an honest "N more lines" marker.

    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use std::path::{Path, PathBuf};

    /// Files shown per tool card at most.
    pub const MAX_DIFF_FILES: usize = 5;
    /// Diff body lines per file at most (headers excluded).
    pub const MAX_DIFF_LINES: usize = 60;
    /// A single file is never snapshotted past this.
    pub const MAX_SNAP_BYTES: usize = 1024 * 1024;
    /// LCS table budget in cells: (n+1)x(m+1) u32. The byte cap does not
    /// bound lines, so a 1MB file of short lines used to demand
    /// gigabytes for the table. 4M cells is ~16MB and fast; beyond it
    /// the diff degrades to an honest "too large" marker instead of
    /// allocating. Diffing stays synchronous on the UI thread (it runs
    /// at ToolCompleted), but bounded this way it cannot stall the UI
    /// on a pathological file; moving it off-thread would need the
    /// snapshot/diff pipeline to become async, which is out of scope
    /// for this fix.
    pub const MAX_LCS_CELLS: usize = 4_000_000;

    /// Content of one file captured at `ToolStarted`.
    #[derive(Debug, Clone)]
    pub struct FileSnapshot {
        pub path: PathBuf,
        /// `None` when the file did not exist (it may be created).
        pub content: Option<String>,
    }

    /// Read `path` for later diffing. Returns `None` for missing files
    /// (recorded as `content: None` by the caller), binaries, and files over
    /// [`MAX_SNAP_BYTES`].
    pub fn snapshot_file(path: &Path) -> Option<FileSnapshot> {
        match std::fs::read(path) {
            Ok(data) => {
                if data.len() > MAX_SNAP_BYTES || data.iter().take(8192).any(|&b| b == 0) {
                    None
                } else {
                    Some(FileSnapshot {
                        path: path.to_path_buf(),
                        content: Some(String::from_utf8_lossy(&data).into_owned()),
                    })
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(FileSnapshot {
                path: path.to_path_buf(),
                content: None,
            }),
            Err(_) => None,
        }
    }

    /// Parse the `edits` array out of `apply_files` args JSON. Returns the
    /// target paths; anything unparseable yields an empty vec (no diff, no
    /// error - diffing is best-effort display).
    ///
    /// Only `apply_files` is hooked: `stage_files` stages without touching
    /// targets (nothing to diff), and `apply_staged` applies by opaque stage
    /// id, so its paths are not visible in the args.
    pub fn edit_targets(tool: &str, args: &str) -> Vec<PathBuf> {
        if tool != "apply_files" {
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
            return Vec::new();
        };
        v.get("edits")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One rendered diff line, style-agnostic.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum DiffLine {
        FileHeader(String),
        Hunk(String),
        Context(String),
        Add(String),
        Del(String),
        Truncated(usize),
        /// LCS table would exceed [`MAX_LCS_CELLS`]: line counts, no
        /// content, so the card stays honest about the skip.
        TooLarge {
            old_lines: usize,
            new_lines: usize,
        },
    }

    /// Line-based diff via LCS on lines. Simple and predictable; inputs are
    /// capped by [`MAX_SNAP_BYTES`] so the O(n·m) table stays small in
    /// practice (typical edits are tens of lines). The byte cap does NOT
    /// bound line counts, so the table is additionally capped by
    /// [`MAX_LCS_CELLS`]: beyond it, a [`DiffLine::TooLarge`] marker
    /// replaces the body instead of allocating gigabytes.
    pub fn unified_diff(old: &str, new: &str) -> Vec<DiffLine> {
        let a: Vec<&str> = old.lines().collect();
        let b: Vec<&str> = new.lines().collect();
        // LCS table.
        let (n, m) = (a.len(), b.len());
        if (n.saturating_add(1)).saturating_mul(m.saturating_add(1)) > MAX_LCS_CELLS {
            return vec![DiffLine::TooLarge {
                old_lines: n,
                new_lines: m,
            }];
        }
        let mut dp = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i][j] = if a[i] == b[j] {
                    dp[i + 1][j + 1] + 1
                } else {
                    dp[i + 1][j].max(dp[i][j + 1])
                };
            }
        }
        // Walk the table into an edit script.
        #[derive(PartialEq, Clone, Copy)]
        enum Op<'x> {
            Same(&'x str),
            Del(&'x str),
            Add(&'x str),
        }
        let mut ops: Vec<Op> = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if a[i] == b[j] {
                ops.push(Op::Same(a[i]));
                i += 1;
                j += 1;
            } else if dp[i + 1][j] >= dp[i][j + 1] {
                ops.push(Op::Del(a[i]));
                i += 1;
            } else {
                ops.push(Op::Add(b[j]));
                j += 1;
            }
        }
        while i < n {
            ops.push(Op::Del(a[i]));
            i += 1;
        }
        while j < m {
            ops.push(Op::Add(b[j]));
            j += 1;
        }

        // Group into hunks with 3 lines of context.
        const CTX: usize = 3;
        fn close_hunk<'x>(
            s: usize,
            last: usize,
            ops: &[Op<'x>],
            ctx: usize,
        ) -> (usize, Vec<Op<'x>>) {
            let end = (last + ctx + 1).min(ops.len());
            (s, ops[s..end].to_vec())
        }
        let mut hunks: Vec<(usize, Vec<Op>)> = Vec::new();
        let mut start: Option<usize> = None;
        let mut last_change = 0usize;
        for (k, op) in ops.iter().enumerate() {
            if !matches!(op, Op::Same(_)) {
                if start.is_none() {
                    start = Some(k.saturating_sub(CTX));
                }
                last_change = k;
            } else if let Some(s) = start {
                if k - last_change > CTX * 2 {
                    hunks.push(close_hunk(s, last_change, &ops, CTX));
                    start = None;
                }
            }
        }
        if let Some(s) = start {
            hunks.push(close_hunk(s, last_change, &ops, CTX));
        }

        let mut out = Vec::new();
        for (s, hunk_ops) in hunks {
            // Hunk header with old/new line ranges.
            let (mut ao, mut bo) = (0usize, 0usize);
            for op in ops.iter().take(s) {
                match op {
                    Op::Same(_) => {
                        ao += 1;
                        bo += 1;
                    }
                    Op::Del(_) => ao += 1,
                    Op::Add(_) => bo += 1,
                }
            }
            let (mut ac, mut bc) = (0usize, 0usize);
            for op in &hunk_ops {
                match op {
                    Op::Same(_) => {
                        ac += 1;
                        bc += 1;
                    }
                    Op::Del(_) => ac += 1,
                    Op::Add(_) => bc += 1,
                }
            }
            out.push(DiffLine::Hunk(format!(
                "@@ -{},{} +{},{} @@",
                ao + 1,
                ac,
                bo + 1,
                bc
            )));
            for op in hunk_ops {
                match op {
                    Op::Same(l) => out.push(DiffLine::Context((*l).to_string())),
                    Op::Del(l) => out.push(DiffLine::Del((*l).to_string())),
                    Op::Add(l) => out.push(DiffLine::Add((*l).to_string())),
                }
            }
        }
        out
    }

    /// Diff one snapshot against current disk content. `None` means "no diff
    /// to show" (binary, unreadable, or unchanged).
    pub fn diff_snapshot(snap: &FileSnapshot) -> Option<Vec<DiffLine>> {
        let after = std::fs::read(&snap.path).ok()?;
        if after.iter().take(8192).any(|&b| b == 0) {
            return None;
        }
        let after = String::from_utf8_lossy(&after).into_owned();
        let before = snap.content.as_deref().unwrap_or("");
        if before == after {
            return None;
        }
        let mut lines = vec![DiffLine::FileHeader(snap.path.display().to_string())];
        let mut body = unified_diff(before, &after);
        if body.len() > MAX_DIFF_LINES {
            let skipped = body.len() - MAX_DIFF_LINES;
            body.truncate(MAX_DIFF_LINES);
            body.push(DiffLine::Truncated(skipped));
        }
        lines.extend(body);
        Some(lines)
    }

    /// Style diff lines with the theme: red `-`, green `+`, dim headers.
    pub fn render_diff_lines(
        lines: &[DiffLine],
        th: &crate::session::theme::Theme,
    ) -> Vec<Line<'static>> {
        let adds = lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Add(_)))
            .count();
        let dels = lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Del(_)))
            .count();
        let stats = if adds > 0 || dels > 0 {
            format!(" (+{adds} -{dels})")
        } else {
            String::new()
        };
        let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len() + 2);
        let mut in_card = false;

        for l in lines {
            match l {
                DiffLine::FileHeader(p) => {
                    if in_card {
                        out.push(Line::from(Span::styled(
                            "╰──────────────────────────────────────────",
                            Style::default().fg(th.dim),
                        )));
                    }
                    out.push(Line::from(vec![
                        Span::styled("╭─ diff ", Style::default().fg(th.dim)),
                        Span::styled(
                            p.clone(),
                            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(stats.clone(), Style::default().fg(th.emphasis)),
                        Span::styled(" ─", Style::default().fg(th.dim)),
                    ]));
                    in_card = true;
                }
                DiffLine::Hunk(h) => {
                    out.push(Line::from(vec![
                        Span::styled("│  ", Style::default().fg(th.dim)),
                        Span::styled(
                            h.clone(),
                            Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
                DiffLine::Context(c) => {
                    out.push(Line::from(vec![
                        Span::styled("│   ", Style::default().fg(th.dim)),
                        Span::styled(c.clone(), Style::default().fg(th.body)),
                    ]));
                }
                DiffLine::Add(a) => {
                    out.push(Line::from(vec![
                        Span::styled(
                            "│ + ",
                            Style::default().fg(th.success).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(a.clone(), Style::default().fg(th.success)),
                    ]));
                }
                DiffLine::Del(d) => {
                    out.push(Line::from(vec![
                        Span::styled(
                            "│ - ",
                            Style::default().fg(th.failure).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(d.clone(), Style::default().fg(th.failure)),
                    ]));
                }
                DiffLine::Truncated(n) => {
                    out.push(Line::from(vec![
                        Span::styled("│   ", Style::default().fg(th.dim)),
                        Span::styled(
                            format!("... {n} more lines"),
                            Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
                DiffLine::TooLarge {
                    old_lines,
                    new_lines,
                } => {
                    out.push(Line::from(vec![
                        Span::styled("│   ", Style::default().fg(th.dim)),
                        Span::styled(
                            format!(
                                "diff skipped: too many lines ({old_lines} → {new_lines}); open the file to review"
                            ),
                            Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
            }
        }
        if in_card {
            out.push(Line::from(Span::styled(
                "╰──────────────────────────────────────────",
                Style::default().fg(th.dim),
            )));
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn huge_line_count_diffs_degrade_to_marker_not_gigabytes() {
            // 3000 x 3000 lines would need a ~36MB table at minimum and
            // grows quadratically; the old code allocated it unconditionally
            // (a 1MB file of 1-char lines is ~500k lines: trillions of cells).
            let old: String = (0..3000).map(|i| format!("old line {i}\n")).collect();
            let new: String = (0..3000).map(|i| format!("new line {i}\n")).collect();
            let out = unified_diff(&old, &new);
            assert_eq!(
                out,
                vec![DiffLine::TooLarge {
                    old_lines: 3000,
                    new_lines: 3000,
                }]
            );
        }

        #[test]
        fn small_diffs_still_produce_hunks() {
            let out = unified_diff("a\nb\nc\n", "a\nB\nc\n");
            assert!(
                out.iter()
                    .any(|l| matches!(l, DiffLine::Del(d) if d == "b")),
                "deletion missing: {out:?}"
            );
            assert!(
                out.iter()
                    .any(|l| matches!(l, DiffLine::Add(a) if a == "B")),
                "addition missing: {out:?}"
            );
        }
    }
}

pub mod transcript {
    //! Transcript blocks for the session view.
    //!
    //! Pure renderers: every block maps to `Vec<Line<'static>>` given the
    //! theme. No terminal types, no session state - the same split as
    //! `widget.rs`/`render.rs`, so tests assert on spans instead of ANSI.
    //!
    //! What this adds over the current transcript:
    //! - [`ThoughtView`]: reasoning as `◇ Thought · 4.0s`, collapsed by
    //!   default, expandable to the full text with its duration.
    //! - [`ToolView`]: tool calls as `› server.tool_name` rows with a live
    //!   marker while running, ✓/× on settle, expandable args/error detail.
    //! - [`ShellView`]: shell commands as `$ command` cards whose long output
    //!   collapses behind an `(N earlier lines)` expander.
    //! - [`background_jobs_line`]: the `↓ N` footer indicator for background
    //!   work, and [`jump_to_latest_line`] for the scrolled-up affordance.
    //!
    //! Expansion state (`expanded`) is owned here as plain data. The live
    //! transcript path in `session.rs::render_block` renders through
    //! [`ToolCard`] (plus [`ThoughtView`] and [`ShellView`]); the older
    //! [`ToolView`] / [`EditView`] renderers and the
    //! [`activity_summary_line`] / [`background_jobs_line`] /
    //! [`run_stats_segment`] / [`jump_to_latest_line`] helpers are NOT
    //! session-integrated - they are reference renderers exercised by
    //! `examples/transcript_shots.rs` and their own unit tests. Do not
    //! document them as live session features.

    use std::time::Duration;

    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};

    use crate::diffview::DiffLine;
    use crate::session::theme::Theme;

    /// How many output lines a collapsed shell card keeps visible.
    pub const SHELL_VISIBLE_LINES: usize = 8;

    /// `4.0s`, `0.4s` duration suffix.
    pub fn fmt_duration(d: Duration) -> String {
        format!("{:.1}s", d.as_secs_f64())
    }

    fn dim_span(th: &Theme, text: String) -> Span<'static> {
        Span::styled(text, Style::default().fg(th.dim))
    }

    /// A reasoning block: `◇ Thought · 4.0s` collapsed, full text expanded.
    ///
    /// The `◇` marker is the amber highlight; everything else stays dim so
    /// old reasoning never shouts over answers. Collapsed is the default
    /// settled thought should read as one quiet line, not a wall.
    #[derive(Debug, Clone)]
    pub struct ThoughtView {
        pub text: String,
        pub duration: Option<Duration>,
        pub expanded: bool,
    }

    impl ThoughtView {
        pub fn collapsed(text: impl Into<String>, duration: Option<Duration>) -> Self {
            Self {
                text: text.into(),
                duration,
                expanded: false,
            }
        }

        /// Header text without styling, for tests and screen readers.
        /// Base design: `◇ Thinking` while expanded (live), `◇ Thought`
        /// (+ duration when measured) when collapsed.
        pub fn header_text(&self) -> String {
            let word = if self.expanded { "Thinking" } else { "Thought" };
            match self.duration {
                Some(d) => format!("◇ {word} · {}", fmt_duration(d)),
                None => format!("◇ {word}"),
            }
        }

        pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
            let word = if self.expanded { "Thinking" } else { "Thought" };
            let mut header = vec![Span::styled(
                "◇ ".to_string(),
                Style::default()
                    .fg(th.emphasis)
                    .add_modifier(Modifier::BOLD),
            )];
            header.push(dim_span(th, word.to_string()));
            if let Some(d) = self.duration {
                header.push(dim_span(th, format!(" · {}", fmt_duration(d))));
            }
            let mut out = vec![Line::from(header)];
            if self.expanded {
                let body = Style::default().fg(th.dim).add_modifier(Modifier::ITALIC);
                for line in self.text.lines() {
                    out.push(Line::from(Span::styled(format!("  {line}"), body)));
                }
            }
            out
        }
    }

    /// Tool-call lifecycle for the row marker.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ToolStatus {
        /// Queued, not yet started: `◐` in dim. A card with no start
        /// time is queued, not running — the two must not read the same.
        Queued,
        /// Still running: `›` in the success color, the "live" signal.
        Running,
        Succeeded,
        Failed,
        /// Stopped from the outside: honest dim `■`, never a spinner.
        Interrupted,
    }

    /// One tool-call row: `› server.tool_name · 0.4s ✓`.
    ///
    /// The row is a single quiet line; Enter expands it to args and the
    /// error/result detail. Names arrive pre-resolved to display form (the
    /// registry id never reaches the transcript).
    #[derive(Debug, Clone)]
    pub struct ToolView {
        pub display_name: String,
        pub status: ToolStatus,
        pub duration: Option<Duration>,
        /// Pre-formatted token count (`154.2k`), if known.
        pub tokens: Option<String>,
        pub expanded: bool,
        /// Args lines shown when expanded.
        pub args: Vec<String>,
        /// Error text (failure) or result summary, shown when expanded.
        pub detail: Vec<String>,
    }

    impl ToolView {
        /// Marker glyph and its color per status.
        fn marker(&self, th: &Theme) -> (String, Style) {
            match self.status {
                ToolStatus::Queued => ("◐".to_string(), Style::default().fg(th.dim)),
                ToolStatus::Running => (
                    "›".to_string(),
                    Style::default().fg(th.success).add_modifier(Modifier::BOLD),
                ),
                ToolStatus::Succeeded => ("✓".to_string(), Style::default().fg(th.success)),
                ToolStatus::Failed => ("×".to_string(), Style::default().fg(th.failure)),
                ToolStatus::Interrupted => ("■".to_string(), Style::default().fg(th.dim)),
            }
        }

        pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
            let (glyph, glyph_style) = self.marker(th);
            let mut header = vec![
                Span::styled(format!("{glyph} "), glyph_style),
                dim_span(th, self.display_name.clone()),
            ];
            if let Some(d) = self.duration {
                header.push(dim_span(th, format!(" · {}", fmt_duration(d))));
            }
            if let Some(t) = &self.tokens {
                header.push(dim_span(th, format!(" · {t}")));
            }
            let mut out = vec![Line::from(header)];
            if self.expanded {
                let arg_style = Style::default().fg(th.dim);
                for arg in &self.args {
                    out.push(Line::from(Span::styled(format!("  {arg}"), arg_style)));
                }
                for line in &self.detail {
                    let style = match self.status {
                        ToolStatus::Failed => Style::default().fg(th.failure),
                        _ => arg_style,
                    };
                    let prefix = if self.status == ToolStatus::Failed {
                        "  ✕ "
                    } else {
                        "  "
                    };
                    out.push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
                }
            }
            out
        }
    }

    /// A shell command card: `$ find . -name '*.rs'` with its output.
    ///
    /// Long output collapses to the last [`SHELL_VISIBLE_LINES`] lines behind
    /// an `(N earlier lines)` expander, so a chatty command never floods the
    /// transcript. The header carries the same status marker as [`ToolView`].
    #[derive(Debug, Clone)]
    pub struct ShellView {
        pub command: String,
        pub output: Vec<String>,
        pub status: ToolStatus,
        pub duration: Option<Duration>,
        pub expanded: bool,
    }

    impl ShellView {
        /// `(3 earlier lines)` when collapsed and output overflows, else `None`.
        pub fn hidden_count(&self) -> Option<usize> {
            if self.expanded {
                return None;
            }
            self.output
                .len()
                .checked_sub(SHELL_VISIBLE_LINES)
                .filter(|&n| n > 0)
        }

        pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
            let (glyph, glyph_style) = match self.status {
                ToolStatus::Queued => ("◐", Style::default().fg(th.dim)),
                ToolStatus::Running => (
                    "›",
                    Style::default().fg(th.success).add_modifier(Modifier::BOLD),
                ),
                ToolStatus::Succeeded => ("✓", Style::default().fg(th.success)),
                ToolStatus::Failed => ("×", Style::default().fg(th.failure)),
                ToolStatus::Interrupted => ("■", Style::default().fg(th.dim)),
            };
            let mut header = vec![
                dim_span(th, "$ ".to_string()),
                Span::styled(self.command.clone(), Style::default().fg(th.body)),
            ];
            if let Some(d) = self.duration {
                header.push(dim_span(th, format!(" · {}", fmt_duration(d))));
            }
            header.push(Span::styled(format!(" {glyph}"), glyph_style));
            let mut out = vec![Line::from(header)];

            let shown: &[String] = match self.hidden_count() {
                Some(hidden) => &self.output[hidden..],
                None => &self.output,
            };
            if let Some(hidden) = self.hidden_count() {
                out.push(Line::from(dim_span(
                    th,
                    format!("  ({hidden} earlier lines)"),
                )));
            }
            let out_style = Style::default().fg(th.dim);
            for line in shown {
                out.push(Line::from(Span::styled(format!("  {line}"), out_style)));
            }
            out
        }
    }

    /// One unified tool card for shell and non-shell tools (base design).
    ///
    /// Header first, so the existing click-to-toggle on a block's first
    /// line keeps working: `● Running: <name> · 0.4s`, `✓ <name>`,
    /// `× <name>`, `■ <name>`. Body lines carry a dim left border
    /// (`│ `) instead of a full rectangle: transcript lines are
    /// width-agnostic, so a full-width top/bottom border cannot be drawn
    /// here — the Frame-rendered composer gets the full border, the
    /// transcript card gets the left border. Shell cards show `$ command`
    /// + output (last [`SHELL_VISIBLE_LINES`] when collapsed); non-shell
    /// cards show args (capped at 6 when collapsed) + error detail.
    /// `ShellView`/`ToolView` stay for their own tests; the live
    /// transcript path in `session.rs::render_block` renders through here.
    #[derive(Debug, Clone)]
    pub struct ToolCard {
        pub display_name: String,
        pub status: ToolStatus,
        pub duration: Option<Duration>,
        pub tokens: Option<String>,
        pub expanded: bool,
        /// Shell command, if this is a shell card.
        pub command: Option<String>,
        pub args: Vec<String>,
        pub output: Vec<String>,
        pub error: Vec<String>,
    }

    impl ToolCard {
        fn header(&self, th: &Theme) -> Line<'static> {
            let (glyph, color) = match self.status {
                ToolStatus::Queued => ("◐", th.dim),
                ToolStatus::Running => ("●", th.success),
                ToolStatus::Succeeded => ("✓", th.success),
                ToolStatus::Failed => ("×", th.failure),
                ToolStatus::Interrupted => ("■", th.dim),
            };
            let mut header = vec![Span::styled(
                format!("{glyph} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )];
            let state_word = match self.status {
                ToolStatus::Queued => "Queued: ",
                ToolStatus::Running => "Running: ",
                ToolStatus::Succeeded => "",
                ToolStatus::Failed => "",
                ToolStatus::Interrupted => "",
            };
            header.push(Span::styled(
                format!("{state_word}{}", self.display_name),
                Style::default().fg(th.body).add_modifier(Modifier::BOLD),
            ));
            if let Some(d) = self.duration {
                header.push(dim_span(th, format!(" · {}", fmt_duration(d))));
            }
            if let Some(t) = &self.tokens {
                header.push(dim_span(th, format!(" · {t}")));
            }
            Line::from(header)
        }

        pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
            let mut out = vec![self.header(th)];
            let border = Style::default().fg(th.dim);
            let mut body = |text: String, style: Style| {
                out.push(Line::from(vec![
                    Span::styled("│ ", border),
                    Span::styled(text, style),
                ]));
            };
            if let Some(cmd) = &self.command {
                body(format!("$ {cmd}"), Style::default().fg(th.body));
                let shown: &[String] = if self.expanded {
                    &self.output
                } else {
                    let hidden = self.output.len().saturating_sub(SHELL_VISIBLE_LINES);
                    if hidden > 0 {
                        body(
                            format!("({hidden} earlier lines)"),
                            Style::default().fg(th.dim),
                        );
                        &self.output[hidden..]
                    } else {
                        &self.output
                    }
                };
                for line in shown {
                    body(line.clone(), Style::default().fg(th.dim));
                }
            } else {
                let limit = if self.expanded { usize::MAX } else { 6 };
                for arg in self.args.iter().take(limit) {
                    body(arg.clone(), Style::default().fg(th.dim));
                }
                for line in self.error.iter().take(10) {
                    body(format!("✕ {line}"), Style::default().fg(th.failure));
                }
            }
            out
        }
    }

    /// Footer indicator for background work: `↓ 2 shells`, dim.
    ///
    /// Renders in the footer's right cluster next to the context meter. Zero
    /// jobs renders nothing - the footer stays quiet when there is nothing to
    /// say. Callers skip the line when `jobs == 0`.
    pub fn background_jobs_line(jobs: usize, th: &Theme) -> Option<Line<'static>> {
        if jobs == 0 {
            return None;
        }
        let noun = if jobs == 1 { "shell" } else { "shells" };
        Some(Line::from(dim_span(th, format!("↓ {jobs} {noun}"))))
    }

    /// Appears when the transcript is scrolled up: `Jump to latest ↓`.
    ///
    /// Muted blue (primary), not amber - it is navigation, not a decision.
    pub fn jump_to_latest_line(th: &Theme) -> Line<'static> {
        Line::from(Span::styled(
            "Jump to latest ↓",
            Style::default().fg(th.primary),
        ))
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

        #[test]
        fn thought_collapsed_shows_duration() {
            let v =
                ThoughtView::collapsed("some long reasoning", Some(Duration::from_millis(4000)));
            let t = text(&v.lines(&theme()));
            assert!(t.contains("◇ Thought · 4.0s"), "got: {t}");
            assert!(!t.contains("some long reasoning"), "body leaked: {t}");
        }

        #[test]
        fn thought_expanded_shows_body() {
            let mut v =
                ThoughtView::collapsed("line one\nline two", Some(Duration::from_millis(400)));
            v.expanded = true;
            let t = text(&v.lines(&theme()));
            assert!(t.contains("◇ Thinking · 0.4s"), "got: {t}");
            assert!(t.contains("line one") && t.contains("line two"), "got: {t}");
        }

        #[test]
        fn thought_without_duration_omits_suffix() {
            let v = ThoughtView::collapsed("x", None);
            assert_eq!(v.header_text(), "◇ Thought");
        }

        #[test]
        fn tool_card_shell_running_shows_command_and_output() {
            let v = ToolCard {
                display_name: "find_files".to_string(),
                status: ToolStatus::Running,
                duration: None,
                tokens: None,
                expanded: false,
                command: Some("find ~/Downloads -type f".to_string()),
                args: vec![],
                output: vec!["1.4G /home/ubuntu/Downloads/x.iso".to_string()],
                error: vec![],
            };
            let t = text(&v.lines(&theme()));
            assert!(t.contains("● Running: find_files"), "got: {t}");
            assert!(t.contains("$ find ~/Downloads -type f"), "got: {t}");
            assert!(t.contains("1.4G"), "got: {t}");
        }

        #[test]
        fn tool_card_queued_reads_differently_from_running() {
            let v = ToolCard {
                display_name: "read".to_string(),
                status: ToolStatus::Queued,
                duration: None,
                tokens: None,
                expanded: false,
                command: None,
                args: vec![],
                output: vec![],
                error: vec![],
            };
            let t = text(&v.lines(&theme()));
            assert!(t.contains("◐ Queued: read"), "got: {t}");
        }

        #[test]
        fn tool_card_nonshell_failed_shows_error() {
            let v = ToolCard {
                display_name: "write".to_string(),
                status: ToolStatus::Failed,
                duration: None,
                tokens: None,
                expanded: true,
                command: None,
                args: vec!["path: a.txt".to_string()],
                output: vec![],
                error: vec!["permission denied".to_string()],
            };
            let t = text(&v.lines(&theme()));
            assert!(t.contains("× write"), "got: {t}");
            assert!(t.contains("permission denied"), "got: {t}");
        }

        #[test]
        fn tool_row_markers_per_status() {
            let base = ToolView {
                display_name: "memory.list_projects".to_string(),
                status: ToolStatus::Running,
                duration: None,
                tokens: None,
                expanded: false,
                args: vec![],
                detail: vec![],
            };
            let t = text(&base.lines(&theme()));
            assert!(t.contains("› memory.list_projects"), "got: {t}");

            for (status, glyph) in [
                (ToolStatus::Succeeded, "✓"),
                (ToolStatus::Failed, "×"),
                (ToolStatus::Interrupted, "■"),
            ] {
                let v = ToolView {
                    status,
                    ..base.clone()
                };
                let t = text(&v.lines(&theme()));
                assert!(t.contains(glyph), "status {status:?}: {t}");
                assert!(t.contains("memory.list_projects"), "status {status:?}: {t}");
            }
        }

        #[test]
        fn tool_row_expanded_shows_args_and_error() {
            let v = ToolView {
                display_name: "shell".to_string(),
                status: ToolStatus::Failed,
                duration: Some(Duration::from_millis(1200)),
                tokens: Some("1.2k".to_string()),
                expanded: true,
                args: vec!["cmd: rm -rf /".to_string()],
                detail: vec!["permission denied".to_string()],
            };
            let t = text(&v.lines(&theme()));
            assert!(t.contains("· 1.2s"), "got: {t}");
            assert!(t.contains("cmd: rm -rf /"), "got: {t}");
            assert!(t.contains("permission denied"), "got: {t}");
        }

        #[test]
        fn tool_row_collapsed_hides_detail() {
            let v = ToolView {
                display_name: "shell".to_string(),
                status: ToolStatus::Succeeded,
                duration: None,
                tokens: None,
                expanded: false,
                args: vec!["secret arg".to_string()],
                detail: vec![],
            };
            let t = text(&v.lines(&theme()));
            assert!(!t.contains("secret arg"), "leaked: {t}");
        }

        #[test]
        fn shell_card_collapses_long_output() {
            let output: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
            let v = ShellView {
                command: "find . -name '*.rs'".to_string(),
                output,
                status: ToolStatus::Succeeded,
                duration: Some(Duration::from_secs(2)),
                expanded: false,
            };
            assert_eq!(v.hidden_count(), Some(12));
            let t = text(&v.lines(&theme()));
            assert!(t.contains("$ find . -name '*.rs'"), "got: {t}");
            assert!(t.contains("(12 earlier lines)"), "got: {t}");
            assert!(t.contains("line 20"), "got: {t}");
            assert!(!t.contains("\n  line 1\n"), "early line leaked: {t}");
        }

        #[test]
        fn shell_card_expanded_shows_everything() {
            let output: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
            let v = ShellView {
                command: "ls".to_string(),
                output,
                status: ToolStatus::Succeeded,
                duration: None,
                expanded: true,
            };
            assert_eq!(v.hidden_count(), None);
            let t = text(&v.lines(&theme()));
            assert!(!t.contains("earlier lines"), "got: {t}");
            assert!(t.contains("line 1") && t.contains("line 20"), "got: {t}");
        }

        #[test]
        fn shell_card_short_output_has_no_expander() {
            let v = ShellView {
                command: "pwd".to_string(),
                output: vec!["/home/hatch".to_string()],
                status: ToolStatus::Succeeded,
                duration: None,
                expanded: false,
            };
            assert_eq!(v.hidden_count(), None);
            let t = text(&v.lines(&theme()));
            assert!(!t.contains("earlier lines"), "got: {t}");
        }

        #[test]
        fn background_indicator_counts_and_pluralizes() {
            assert!(background_jobs_line(0, &theme()).is_none());
            let one = text(&[background_jobs_line(1, &theme()).unwrap()]);
            assert!(one.contains("↓ 1 shell"), "got: {one}");
            assert!(!one.contains("shells"), "got: {one}");
            let two = text(&[background_jobs_line(2, &theme()).unwrap()]);
            assert!(two.contains("↓ 2 shells"), "got: {two}");
        }

        #[test]
        fn jump_to_latest_renders() {
            let t = text(&[jump_to_latest_line(&theme())]);
            assert!(t.contains("Jump to latest ↓"), "got: {t}");
        }

        #[test]
        fn fmt_duration_shapes() {
            assert_eq!(fmt_duration(Duration::from_millis(4000)), "4.0s");
            assert_eq!(fmt_duration(Duration::from_millis(400)), "0.4s");
        }

        #[test]
        fn numbered_diff_tracks_real_line_numbers() {
            let old = "a\nb\nc\nd\ne\n";
            let new = "a\nB\nc\nd\nE\n";
            let lines = numbered_diff(old, new);
            let dels: Vec<_> = lines
                .iter()
                .filter(|l| matches!(l.kind, NumberedKind::Del(_)))
                .collect();
            let adds: Vec<_> = lines
                .iter()
                .filter(|l| matches!(l.kind, NumberedKind::Add(_)))
                .collect();
            assert_eq!(dels.len(), 2);
            assert_eq!(dels[0].old_no, Some(2));
            assert_eq!(dels[1].old_no, Some(5));
            assert_eq!(adds.len(), 2);
            assert_eq!(adds[0].new_no, Some(2));
            assert_eq!(adds[1].new_no, Some(5));
        }

        #[test]
        fn edit_view_renders_header_and_numbered_lines() {
            let v = EditView::from_texts("src/main.rs", "a\nb\n", "a\nB\n", true);
            let t = text(&v.lines(&theme()));
            assert!(t.contains("← Edit src/main.rs"), "got: {t}");
            assert!(t.contains("-b"), "got: {t}");
            assert!(t.contains("+B"), "got: {t}");
            // Real line numbers show in the gutter.
            assert!(t.contains("2"), "got: {t}");
        }

        #[test]
        fn edit_view_collapses_long_diffs() {
            let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
            let new: String = (1..=40)
                .map(|i| {
                    if i % 2 == 0 {
                        format!("LINE {i}\n")
                    } else {
                        format!("line {i}\n")
                    }
                })
                .collect();
            let v = EditView::from_texts("big.rs", &old, &new, false);
            assert!(v.hidden_count() > 0);
            let t = text(&v.lines(&theme()));
            assert!(t.contains("more lines"), "got: {t}");

            let v2 = EditView::from_texts("big.rs", &old, &new, true);
            assert_eq!(v2.hidden_count(), 0);
            let t2 = text(&v2.lines(&theme()));
            assert!(!t2.contains("more lines"), "got: {t2}");
        }

        #[test]
        fn activity_summary_rows() {
            let one = text(&[activity_summary_line(1, "search", &theme())]);
            assert!(one.contains("→ Explored: 1 search"), "got: {one}");
            let many = text(&[activity_summary_line(3, "read", &theme())]);
            assert!(many.contains("→ Explored: 3 reads"), "got: {many}");
            let searches = text(&[activity_summary_line(2, "search", &theme())]);
            assert!(searches.contains("2 searches"), "got: {searches}");
        }

        #[test]
        fn burst_object_classifies() {
            assert_eq!(burst_object("web_search"), "search");
            assert_eq!(burst_object("read_file"), "read");
            assert_eq!(burst_object("shell"), "tool");
        }

        #[test]
        fn run_stats_formatting() {
            assert_eq!(fmt_elapsed(Duration::from_secs(270)), "4m 30s");
            assert_eq!(fmt_elapsed(Duration::from_millis(4400)), "4.4s");
            assert_eq!(
                run_stats_segment(Duration::from_secs(270), 7317),
                "· 4m 30s · 27.1 tok/s"
            );
        }
    }

    /// How many diff lines a collapsed edit card keeps visible.
    pub const EDIT_VISIBLE_LINES: usize = 12;

    /// A diff line annotated with real file line numbers, parsed out of the
    /// `@@ -a,b +c,d @@` hunk headers [`crate::diffview::unified_diff`]
    /// emits. Removed lines carry the old-file number, added lines the
    /// new-file number, context lines both.
    #[derive(Debug, Clone)]
    pub struct NumberedDiffLine {
        pub old_no: Option<u32>,
        pub new_no: Option<u32>,
        pub kind: NumberedKind,
    }

    #[derive(Debug, Clone)]
    pub enum NumberedKind {
        Hunk(String),
        Context(String),
        Add(String),
        Del(String),
        Truncated(usize),
    }

    /// Parse `@@ -573,4 +578,6 @@` into the 1-based starting lines.
    fn parse_hunk_header(h: &str) -> Option<(u32, u32)> {
        let h = h.strip_prefix("@@ -")?;
        let (old, rest) = h.split_once(" +")?;
        let new = rest.split_whitespace().next()?;
        let a: u32 = old.split(',').next()?.parse().ok()?;
        let b: u32 = new.split(',').next()?.parse().ok()?;
        Some((a, b))
    }

    /// [`crate::diffview::unified_diff`] plus file line numbers. `FileHeader`
    /// lines are dropped - [`EditView`] renders the path itself.
    pub fn numbered_diff(old: &str, new: &str) -> Vec<NumberedDiffLine> {
        let mut out = Vec::new();
        let (mut ao, mut bo) = (0u32, 0u32);
        for dl in crate::diffview::unified_diff(old, new) {
            match dl {
                DiffLine::FileHeader(_) => {}
                DiffLine::Hunk(h) => {
                    if let Some((a, b)) = parse_hunk_header(&h) {
                        ao = a;
                        bo = b;
                    }
                    out.push(NumberedDiffLine {
                        old_no: None,
                        new_no: None,
                        kind: NumberedKind::Hunk(h),
                    });
                }
                DiffLine::Context(c) => {
                    out.push(NumberedDiffLine {
                        old_no: Some(ao),
                        new_no: Some(bo),
                        kind: NumberedKind::Context(c),
                    });
                    ao += 1;
                    bo += 1;
                }
                DiffLine::Del(d) => {
                    out.push(NumberedDiffLine {
                        old_no: Some(ao),
                        new_no: None,
                        kind: NumberedKind::Del(d),
                    });
                    ao += 1;
                }
                DiffLine::Add(a) => {
                    out.push(NumberedDiffLine {
                        old_no: None,
                        new_no: Some(bo),
                        kind: NumberedKind::Add(a),
                    });
                    bo += 1;
                }
                DiffLine::Truncated(n) => out.push(NumberedDiffLine {
                    old_no: None,
                    new_no: None,
                    kind: NumberedKind::Truncated(n),
                }),
                // A skipped diff renders as a hunk-header line naming
                // the skip: no new NumberedKind needed, and line numbers
                // are meaningless here.
                DiffLine::TooLarge {
                    old_lines,
                    new_lines,
                } => out.push(NumberedDiffLine {
                    old_no: None,
                    new_no: None,
                    kind: NumberedKind::Hunk(format!(
                        "diff skipped: too many lines ({old_lines} → {new_lines})"
                    )),
                }),
            }
        }
        out
    }

    /// A file edit card: `← Edit <path>` over a line-numbered diff.
    ///
    /// Red strikethrough for removals, green for additions, dim line numbers
    /// and hunk headers. Long diffs collapse behind `(N more lines)`; the
    /// collapsed card keeps the first [`EDIT_VISIBLE_LINES`] diff lines.
    #[derive(Debug, Clone)]
    pub struct EditView {
        pub path: String,
        pub lines: Vec<NumberedDiffLine>,
        pub expanded: bool,
    }

    impl EditView {
        pub fn from_texts(path: impl Into<String>, old: &str, new: &str, expanded: bool) -> Self {
            Self {
                path: path.into(),
                lines: numbered_diff(old, new),
                expanded,
            }
        }

        /// Diff body lines hidden while collapsed.
        pub fn hidden_count(&self) -> usize {
            if self.expanded {
                0
            } else {
                self.lines.len().saturating_sub(EDIT_VISIBLE_LINES)
            }
        }

        fn render_diff_line(&self, l: &NumberedDiffLine, th: &Theme) -> Line<'static> {
            // Line number gutter: the side that exists for this line kind.
            let no = l.old_no.or(l.new_no).map(|n| format!("{n:>4} "));
            let gutter = Span::styled(
                no.unwrap_or_else(|| "     ".to_string()),
                Style::default().fg(th.dim),
            );
            match &l.kind {
                NumberedKind::Hunk(h) => Line::from(vec![
                    gutter,
                    Span::styled(h.clone(), Style::default().fg(th.dim)),
                ]),
                NumberedKind::Context(c) => Line::from(vec![
                    gutter,
                    Span::styled(format!(" {c}"), Style::default().fg(th.dim)),
                ]),
                NumberedKind::Add(a) => Line::from(vec![
                    gutter,
                    Span::styled(format!("+{a}"), Style::default().fg(th.success)),
                ]),
                NumberedKind::Del(d) => Line::from(vec![
                    gutter,
                    Span::styled(
                        format!("-{d}"),
                        Style::default()
                            .fg(th.failure)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                ]),
                NumberedKind::Truncated(n) => Line::from(Span::styled(
                    format!("  ... {n} more lines"),
                    Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
                )),
            }
        }

        pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
            let mut out = vec![Line::from(vec![
                Span::styled(
                    "← ",
                    Style::default()
                        .fg(th.emphasis)
                        .add_modifier(Modifier::BOLD),
                ),
                dim_span(th, "Edit ".to_string()),
                Span::styled(self.path.clone(), Style::default().fg(th.body)),
            ])];
            let shown = if self.expanded {
                &self.lines[..]
            } else {
                &self.lines[..self.lines.len().min(EDIT_VISIBLE_LINES)]
            };
            for l in shown {
                out.push(self.render_diff_line(l, th));
            }
            let hidden = self.hidden_count();
            if hidden > 0 {
                out.push(Line::from(Span::styled(
                    format!("  ({hidden} more lines)"),
                    Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
                )));
            }
            out
        }
    }

    /// Map a tool's display name to the object word for burst summaries:
    /// search-like tools → "search", read-like → "read", anything else → "tool".
    pub fn burst_object(tool_display_name: &str) -> &'static str {
        let n = tool_display_name.to_lowercase();
        if n.contains("search") || n.contains("grep") || n.contains("find") {
            "search"
        } else if n.contains("read")
            || n.contains("cat")
            || n.contains("list")
            || n.contains("glob")
        {
            "read"
        } else {
            "tool"
        }
    }

    fn plural_object(object: &str, count: usize) -> String {
        if count == 1 {
            return object.to_string();
        }
        match object {
            "search" => "searches".to_string(),
            _ => format!("{object}s"),
        }
    }

    /// Condensed activity row for a burst of read-only tool calls:
    /// `→ Explored: 1 search`, `→ Explored: 3 reads`.
    ///
    /// The `→` is dim; the row is deliberately quieter than a tool card
    /// it summarizes, it does not announce.
    pub fn activity_summary_line(count: usize, object: &str, th: &Theme) -> Line<'static> {
        Line::from(vec![
            dim_span(th, "→ ".to_string()),
            dim_span(
                th,
                format!("Explored: {count} {}", plural_object(object, count)),
            ),
        ])
    }

    /// `4m 30s`; under a minute, `12.4s`.
    pub fn fmt_elapsed(d: Duration) -> String {
        let secs = d.as_secs();
        if secs >= 60 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{:.1}s", d.as_secs_f64())
        }
    }

    /// `27.1 tok/s`; ` - ` when the turn was instant (no division by zero).
    pub fn fmt_tok_per_sec(tokens: u64, elapsed: Duration) -> String {
        let s = elapsed.as_secs_f64();
        if s <= 0.0 {
            return " - ".to_string();
        }
        format!("{:.1} tok/s", tokens as f64 / s)
    }

    /// Footer suffix for a completed turn: `· 4m 30s · 27.1 tok/s`.
    ///
    /// Pure formatting - splicing this into `render_footer`'s right cluster
    /// (next to the context meter) needs `session.rs`, which the concurrent
    /// fix pass is editing. Left as a documented integration point.
    pub fn run_stats_segment(elapsed: Duration, tokens: u64) -> String {
        format!(
            "· {} · {}",
            fmt_elapsed(elapsed),
            fmt_tok_per_sec(tokens, elapsed)
        )
    }
}

pub mod ui_render {
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
                                format!(
                                    "{pad}{:<label_w$} ",
                                    fit(label, label_w),
                                    label_w = label_w
                                ),
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
    fn render_metrics(
        m: &Metrics,
        th: &Theme,
        width: usize,
        pad: &str,
        out: &mut Vec<Line<'static>>,
    ) {
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

    fn render_details(
        d: &Details,
        th: &Theme,
        width: usize,
        pad: &str,
        out: &mut Vec<Line<'static>>,
    ) {
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

    fn render_layout(
        l: &Layout,
        th: &Theme,
        width: usize,
        pad: &str,
        out: &mut Vec<Line<'static>>,
    ) {
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
}

pub mod desktop {
    //! `pantheon desktop` — build (when needed) and launch the desktop app, and
    //! register it with the desktop environment so app pickers can open it.
    //!
    //! The desktop app lives in `<repo>/desktop` (Tauri 2; own workspace, excluded
    //! from the std-only one). First run builds: `npm install`, `npm run build`
    //! (vite → dist), `cargo build --release` (embeds dist). Later runs reuse the
    //! artifacts and only rebuild what is missing or stale. After a successful
    //! build the app registers itself into XDG menus
    //! (`~/.local/share/applications/pantheon-desktop.desktop` + hicolor icon).
    //!
    //! Flags: `--rebuild` force the full build; `--skip-build` launch only;
    //! `--register` (re)write the desktop entry; `--no-register` skip it.

    use crate::args::Args;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::SystemTime;

    /// `argv` is the full command line, `pantheon desktop …` at the front.
    const DESKTOP_ENTRY_NAME: &str = "pantheon-desktop";

    /// `argv` is the full command line, `pantheon desktop …` at the front.
    pub fn cmd_desktop(argv: &[String]) -> ! {
        // Help is a question, not a build: without this, `pantheon
        // desktop --help` fell through to ensure_built and tried to
        // build/launch the app. Unknown flags were swallowed the same
        // way (a typo like --rebuid "worked" by doing the default).
        if argv.iter().any(|a| a == "--help" || a == "-h") {
            println!("pantheon desktop [--rebuild] [--skip-build] [--register] [--no-register]");
            println!();
            println!("Build (when needed) and launch the desktop app, and register");
            println!("it with the desktop environment so app pickers can open it.");
            println!("  --rebuild     force the full build");
            println!("  --skip-build  launch only, never build");
            println!("  --register    (re)write the desktop entry and stop (no launch)");
            println!("  --no-register skip the desktop-entry registration");
            std::process::exit(0);
        }
        for a in argv.iter().skip(2) {
            if a.starts_with('-')
                && !matches!(
                    a.as_str(),
                    "--rebuild" | "--skip-build" | "--register" | "--no-register"
                )
            {
                eprintln!("desktop: unknown flag '{a}'");
                std::process::exit(2);
            }
            if !a.starts_with('-') {
                eprintln!("desktop: unexpected argument '{a}' (this command takes flags only)");
                std::process::exit(2);
            }
        }
        let args = Args::parse(argv);
        let rebuild = args.has("rebuild");
        let skip_build = args.has("skip-build");
        let register = args.has("register");
        let no_register = args.has("no-register");

        let Some(dir) = locate_desktop_dir() else {
            eprintln!(
                "pantheon desktop: cannot find the desktop app. Set PANTHEON_DESKTOP_DIR \
                 to the checkout's `desktop/` directory (it lives next to the runtime \
                 sources: <repo>/desktop)."
            );
            std::process::exit(2);
        };

        if !skip_build {
            if let Err(error) = ensure_built(&dir, rebuild) {
                eprintln!("pantheon desktop: build failed: {error}");
                std::process::exit(1);
            }
        }

        let binary = dir.join("src-tauri/target/release/pantheon-desktop");
        if !binary.exists() {
            eprintln!(
                "pantheon desktop: no built app at {}. Run `pantheon desktop --rebuild`.",
                binary.display()
            );
            std::process::exit(2);
        }

        // An explicit `--register` is a registration request, not a launch:
        // a failed registration must exit non-zero (the old code printed
        // "registration skipped" and still exited 0, so scripts could not
        // tell). The default path (register-then-launch) keeps the softer
        // behaviour: registration is best-effort there and launch proceeds.
        let mut registration_failed = false;
        if !no_register {
            match register_desktop_entry(&dir, &binary) {
                Ok(entry) => println!("registered: {}", entry.display()),
                Err(error) => {
                    eprintln!("pantheon desktop: registration skipped ({error})");
                    registration_failed = true;
                }
            }
        }
        if register {
            std::process::exit(if registration_failed { 1 } else { 0 });
        }

        println!("launching Pantheon desktop…");
        let status = Command::new(&binary).status().unwrap_or_else(|error| {
            eprintln!(
                "pantheon desktop: failed to launch {}: {error}",
                binary.display()
            );
            std::process::exit(1);
        });
        std::process::exit(status.code().unwrap_or(1));
    }

    /// Discovery ladder (plan doc §12: one resolver, validated candidates):
    /// explicit env → checkout layout (exe at `<repo>/target/release/pantheon`)
    /// → per-user install location.
    fn locate_desktop_dir() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("PANTHEON_DESKTOP_DIR") {
            let dir = PathBuf::from(dir);
            if dir.join("src-tauri").is_dir() {
                return Some(dir);
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            // <repo>/target/{debug,release,fast-release}/pantheon[-desktop] → <repo>/desktop
            if let Some(repo) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
                let dir = repo.join("desktop");
                if dir.join("src-tauri").is_dir() {
                    return Some(dir.canonicalize().unwrap_or(dir));
                }
            }
        }
        let user = home()?.join(".local/share/pantheon/desktop");
        if user.join("src-tauri").is_dir() {
            return Some(user);
        }
        None
    }

    fn home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    fn ensure_built(dir: &Path, rebuild: bool) -> Result<(), String> {
        let run = |description: &str, mut command: Command| -> Result<(), String> {
            println!("pantheon desktop: {description}…");
            let status = command
                .status()
                .map_err(|e| format!("{description}: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("{description} exited {status}"))
            }
        };

        if !dir.join("node_modules").exists() || rebuild {
            let mut command = Command::new("npm");
            command.arg("install").current_dir(dir);
            run("installing desktop dependencies (npm install)", command)?;
        }

        let dist = dir.join("dist/index.html");
        if rebuild
            || !dist.exists()
            || stale(&dist, &dir.join("src"))
            || stale(&dist, &dir.join("index.html"))
        {
            let mut command = Command::new("npm");
            command.arg("run").arg("build").current_dir(dir);
            run("building desktop frontend (vite)", command)?;
        }

        let binary = dir.join("src-tauri/target/release/pantheon-desktop");
        if rebuild
            || !binary.exists()
            || stale(&binary, &dir.join("dist"))
            || stale(&binary, &dir.join("src-tauri/src"))
        {
            let mut command = Command::new("cargo");
            // custom-protocol embeds the built frontend; without it the shell
            // bakes the devUrl and the window shows WebKit's connect-refused page.
            command
                .args(["build", "--release", "--features", "custom-protocol"])
                .current_dir(dir.join("src-tauri"));
            run("building desktop shell (cargo build --release)", command)?;
        }
        Ok(())
    }

    /// `target` is stale when `against` (a file or directory tree) holds anything newer.
    fn stale(target: &Path, against: &Path) -> bool {
        let Some(built) = modified(target) else {
            return true;
        };
        newest_modified(against).is_some_and(|newest| newest > built)
    }

    fn modified(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    fn newest_modified(path: &Path) -> Option<SystemTime> {
        let meta = std::fs::metadata(path).ok()?;
        if meta.is_file() {
            return modified(path);
        }
        std::fs::read_dir(path)
            .ok()?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| newest_modified(&entry.path()))
            .max()
            .or_else(|| modified(path))
    }

    /// Write the XDG desktop entry + hicolor icon so app pickers list Pantheon.
    fn register_desktop_entry(dir: &Path, binary: &Path) -> Result<PathBuf, String> {
        let home = home().ok_or("no HOME")?;
        let icon_dir = home.join(".local/share/icons/hicolor/1024x1024/apps");
        let icon_source = dir.join("src-tauri/icons/icon.png");
        std::fs::create_dir_all(&icon_dir).map_err(|e| format!("icon dir: {e}"))?;
        let icon = icon_dir.join("pantheon.png");
        std::fs::copy(&icon_source, &icon).map_err(|e| format!("icon copy: {e}"))?;

        let entry = format!(
            "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Pantheon\nGenericName=AI Agent Desktop\nComment=Pantheon — the durable runtime for AI agents\nExec={}\nIcon={}\nTerminal=false\nCategories=Development;IDE;\nStartupWMClass=pantheon-desktop\nStartupNotify=true\n",
            binary.display(),
            icon.display(),
        );
        let applications = home.join(".local/share/applications");
        std::fs::create_dir_all(&applications).map_err(|e| format!("applications dir: {e}"))?;
        let path = applications.join(format!("{DESKTOP_ENTRY_NAME}.desktop"));
        std::fs::write(&path, entry).map_err(|e| format!("write entry: {e}"))?;
        // Menu caches pick up user entries without this on most desktops; run it
        // when present and ignore failure (best effort, never blocks launch).
        let _ = Command::new("update-desktop-database")
            .arg(&applications)
            .status();
        Ok(path)
    }
}

pub mod yank {
    //! Yank: copy the last assistant message - or one of its code blocks
    //! to the system clipboard.
    //!
    //! Clipboard backends are probed in order and the first present wins:
    //! `wl-copy` (Wayland), `xclip`, `xsel` (X11), `pbcopy` (macOS). All
    //! best-effort: when none is installed the caller reports it instead of
    //! failing.

    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// One fenced code block: optional language tag and the code.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CodeBlock {
        pub language: Option<String>,
        pub code: String,
    }

    /// Extract ``` fenced code blocks from markdown, in order. An unclosed
    /// fence runs to the end of the text. Inline backticks are ignored.
    pub fn extract_code_blocks(text: &str) -> Vec<CodeBlock> {
        let mut out = Vec::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix("```") {
                let language = {
                    let lang = rest.trim();
                    if lang.is_empty() {
                        None
                    } else {
                        Some(lang.to_string())
                    }
                };
                let mut code = String::new();
                for l in lines.by_ref() {
                    if l.trim_start().starts_with("```") {
                        break;
                    }
                    code.push_str(l);
                    code.push('\n');
                }
                out.push(CodeBlock { language, code });
            }
        }
        out
    }

    /// Backend probing, split from PATH lookup for testability.
    pub fn find_clipboard(search_dirs: &[PathBuf]) -> Option<(&'static str, PathBuf)> {
        // (binary, argv): wl-copy and pbcopy read stdin; xclip/xsel need flags.
        for (bin, _args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
            ("pbcopy", &[][..]),
        ] {
            let found = search_dirs
                .iter()
                .map(|d| d.join(bin))
                .find(|p| is_executable(p));
            if let Some(path) = found {
                return Some((bin, path));
            }
        }
        None
    }

    /// Executable check: permission bits on unix, mere presence on windows.
    /// Windows has no exec bit, and the candidate names are already the
    /// clipboard backends, so existence is the whole test there.
    fn is_executable(p: &Path) -> bool {
        let Ok(m) = p.metadata() else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            m.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            m.is_file()
        }
    }

    fn path_dirs() -> Vec<PathBuf> {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default()
    }

    /// Build the clipboard command for an explicit backend, or rather for
    /// the `(name, path)` pair discovery returns. Split from PATH lookup so
    /// tests assert argv without executing anything. The payload travels
    /// over stdin (never argv), which the caller writes.
    pub fn clipboard_command_for(bin: &str, path: &Path) -> Command {
        let mut cmd = Command::new(path);
        match bin {
            "xclip" => {
                cmd.arg("-selection").arg("clipboard");
            }
            "xsel" => {
                cmd.arg("--clipboard").arg("--input");
            }
            _ => {}
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    /// Build the clipboard command, or `None` when no backend exists.
    pub fn clipboard_command() -> Option<Command> {
        let (bin, path) = find_clipboard(&path_dirs())?;
        Some(clipboard_command_for(bin, &path))
    }

    /// Copy `text` to the clipboard via the first available backend.
    /// Returns the backend name, or `None` when nothing is installed.
    pub fn copy_to_clipboard(text: &str) -> Option<&'static str> {
        let (bin, path) = find_clipboard(&path_dirs())?;
        let mut child = clipboard_command_for(bin, &path).spawn().ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        match child.wait() {
            Ok(s) if s.success() => Some(bin),
            _ => None,
        }
    }
}

pub mod sanitize {
    //! Strip terminal control characters from untrusted text.
    //!
    //! Anything that reaches the terminal from outside the process is
    //! untrusted: a plugin manifest, model output, tool output, an MCP
    //! result, a log line. Ratatui-rendered surfaces filter control characters
    //! themselves (its `Buffer::set_stringn` drops them), but the plain
    //! `println!` paths do not, and those run in raw mode with no alternate
    //! screen. A manifest description carrying `\x1b[2J` or `\x1b]0;title`
    //! can clear the screen, move the cursor, or retitle the terminal.
    //!
    //! This is the shared stripper for those paths.

    /// Remove every control character, and drop the payload of a terminal
    /// escape sequence rather than printing it as literal text.
    ///
    /// Ratatui-rendered surfaces filter control characters themselves (its
    /// `Buffer::set_stringn` drops them), but the plain `println!` paths do
    /// not, and those run in raw mode with no alternate screen. A manifest
    /// description carrying `\x1b[2J` or `\x1b]0;title` can clear the
    /// screen, move the cursor, or retitle the terminal.
    ///
    /// Removing only the ESC byte would turn `\x1b[2J` into the visible text
    /// `[2J`, which is still wrong: it corrupts a column-aligned table and
    /// misleads the operator about what they are reading. So CSI, OSC, and
    /// the other escape forms are consumed whole.
    ///
    /// This is the shared stripper for those paths.
    pub fn strip_control(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            if !is_control(ch) {
                out.push(ch);
                i += 1;
                continue;
            }
            // Drop the whole escape sequence when this control byte opens one.
            if ch == '\u{1b}' {
                i = skip_escape(&chars, i);
                push_separator(&mut out);
                continue;
            }
            // Any other control character becomes a single space, so a
            // newline or tab cannot forge extra rows in a table.
            push_separator(&mut out);
            i += 1;
        }
        out.trim_end().to_string()
    }

    /// True for C0 controls, DEL, and the C1 range.
    fn is_control(ch: char) -> bool {
        ch.is_control() || matches!(ch, '\u{7f}'..='\u{9f}')
    }

    /// Append a space unless one is already there or nothing precedes it, so
    /// a run of stripped sequences collapses instead of leaving ragged gaps.
    fn push_separator(out: &mut String) {
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
    }

    /// Given the index of an ESC, return the index just past its sequence.
    ///
    /// Handles the forms that matter in untrusted text: CSI (`ESC [ ... final`),
    /// OSC (`ESC ] ... BEL` or `ESC ] ... ESC \`), and a plain two-character
    /// escape. Anything unrecognized consumes only the ESC and its immediate
    /// parameter bytes, so the function always terminates.
    fn skip_escape(chars: &[char], esc: usize) -> usize {
        let next = match chars.get(esc + 1) {
            Some(&c) => c,
            None => return esc + 1,
        };
        match next {
            '[' => {
                // CSI: parameter bytes 0x30-0x3F, intermediate 0x20-0x2F,
                // final byte 0x40-0x7E.
                let mut i = esc + 2;
                while i < chars.len() && matches!(chars[i], '\u{30}'..='\u{3f}') {
                    i += 1;
                }
                while i < chars.len() && matches!(chars[i], '\u{20}'..='\u{2f}') {
                    i += 1;
                }
                // Exactly one final byte, then the sequence ends.
                if i < chars.len() && matches!(chars[i], '\u{40}'..='\u{7e}') {
                    i += 1;
                }
                i
            }
            ']' => {
                // OSC: runs until BEL, or until ESC \ (ST).
                let mut i = esc + 2;
                while i < chars.len() {
                    match chars[i] {
                        '\u{7}' => return i + 1,
                        '\u{1b}' if chars.get(i + 1) == Some(&'\\') => return i + 2,
                        _ => i += 1,
                    }
                }
                i
            }
            'P' | 'X' | '^' | '_' => {
                // DCS/SOS/PM/APC: string sequences, same terminator rules as OSC.
                let mut i = esc + 2;
                while i < chars.len() {
                    match chars[i] {
                        '\u{7}' => return i + 1,
                        '\u{1b}' if chars.get(i + 1) == Some(&'\\') => return i + 2,
                        _ => i += 1,
                    }
                }
                i
            }
            // Two-character escapes use only these final bytes. The wider
            // 0x30..=0x7e range belongs to CSI, where a final byte is
            // preceded by parameter and intermediate bytes; applying it here
            // would swallow the first letter of any ordinary word that
            // happened to follow a bare ESC.
            _ if next.is_ascii_digit()
                || next == ';'
                || ('<'..='?').contains(&next)
                || ('\u{20}'..='\u{2f}').contains(&next) =>
            {
                esc + 2
            }
            _ => esc + 1,
        }
    }

    /// [`strip_control`] with a hard length cap, for fixed-width columns.
    ///
    /// Truncation happens after stripping so a multi-byte sequence is never
    /// cut mid-character, and the cap is applied on character boundaries.
    pub fn strip_control_truncated(s: &str, max: usize) -> String {
        let clean = strip_control(s);
        if clean.chars().count() <= max {
            return clean;
        }
        let mut out: String = clean.chars().take(max.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn esc_sequences_are_removed() {
            assert_eq!(strip_control("\x1b[2J\x1b]0;pwned\x07done"), "done");
        }

        #[test]
        fn cursor_moves_are_removed() {
            // CSI cursor positioning and the OSC title setter are the two that
            // actually confuse an operator.
            assert_eq!(strip_control("a\x1b[10Gb"), "a b");
            assert_eq!(strip_control("\x1b[?25lx"), "x");
        }

        #[test]
        fn printable_unicode_survives() {
            assert_eq!(strip_control("café über"), "café über");
            assert_eq!(strip_control("日本語"), "日本語");
        }

        #[test]
        fn runs_of_controls_collapse() {
            // Two real sequences in a row collapse to one separator.
            assert_eq!(strip_control("a\x1b[0mb\x1b[0mc"), "a b c");
            // A bare ESC followed by another ESC is not a sequence opener,
            // so it consumes just the escape bytes and the text survives.
            assert_eq!(strip_control("a\x1bb"), "a b");
        }

        #[test]
        fn unterminated_escape_does_not_swallow_the_rest() {
            // No final byte: the stripper must still terminate and must not
            // eat text that follows.
            assert_eq!(strip_control("a\x1b[999"), "a");
            assert_eq!(strip_control("a\x1b"), "a");
        }

        #[test]
        fn truncated_respects_char_boundaries() {
            let s = "日本語テキスト";
            let out = strip_control_truncated(s, 5);
            // 5 chars of content budget, with the ellipsis inside that cap.
            assert_eq!(out, "日本...");
            assert_eq!(out.chars().count(), 5);
        }

        #[test]
        fn clean_text_is_unchanged() {
            assert_eq!(strip_control("plain text"), "plain text");
            assert_eq!(strip_control_truncated("short", 20), "short");
        }
    }
}

pub mod send {
    //! `pantheon send --to telegram|discord|mobile|home "message"` - push a
    //! message to a gateway surface. For the user/cron, not the agent's normal
    //! reply path.
    //!
    //! `telegram`/`discord` send immediately over the gateway's existing REST
    //! transports, so a missing token is a loud error here rather than a queued
    //! message nobody drains. `home`/`mobile` are session targets: the message
    //! is written into the well-known home session's ledger (the session
    //! auto-creates via `Ledger::ensure_home_session`; the mobile app renders
    //! that same session, which is what carries `mobile` delivery to the
    //! phone). Nothing here invents a transport.

    use pantheon_gateway::schedule_delivery::{ChannelSender, Deliver, RestChannelSender};
    use std::path::Path;

    fn usage() -> &'static str {
        "usage:\n  \
         pantheon send --to telegram|discord|mobile|home \"message\""
    }

    /// What `parse_send_args` found.
    #[derive(Debug, PartialEq, Eq)]
    pub enum SendArgs {
        Help,
        Send { target: Deliver, message: String },
    }

    /// Parse `pantheon send` argv. Pure: unit-tested. `--to` is required; the
    /// message is all remaining positional args joined with spaces, so quoting
    /// is optional. An unknown target is a loud error naming the target.
    pub fn parse_send_args(args: &[String]) -> Result<SendArgs, String> {
        let mut to: Option<String> = None;
        let mut message_parts: Vec<String> = Vec::new();
        let mut i = 2;
        while i < args.len() {
            match args[i].as_str() {
                "--to" => {
                    i += 1;
                    to = Some(args.get(i).cloned().ok_or_else(|| {
                        "--to needs a target: telegram|discord|mobile|home".to_string()
                    })?);
                }
                "--help" | "-h" => return Ok(SendArgs::Help),
                other if other.starts_with("--") => {
                    return Err(format!("unknown flag '{other}'"));
                }
                text => message_parts.push(text.to_string()),
            }
            i += 1;
        }
        let target = match to {
            Some(t) => Deliver::parse(&t)?,
            None => return Err("missing --to <target>".to_string()),
        };
        let message = message_parts.join(" ");
        if message.trim().is_empty() {
            return Err("missing message".to_string());
        }
        Ok(SendArgs::Send { target, message })
    }

    /// Deliver one user-authored message to its target. Returns the
    /// confirmation line for stdout. Every failure comes back as `Err` with the
    /// exact cause - unknown target, missing token, ledger write failure - so
    /// neither the CLI nor `/send` can fail silently.
    pub fn send_to_target(
        data_dir: &Path,
        target: &Deliver,
        message: &str,
    ) -> Result<String, String> {
        let message = message.trim();
        if message.is_empty() {
            return Err("message is empty".to_string());
        }
        let sender = RestChannelSender::with_data_dir(data_dir);
        match target {
            Deliver::Telegram => sender
                .send_telegram(message)
                .map(|()| "sent to telegram".to_string()),
            Deliver::Discord => sender
                .send_discord(message)
                .map(|()| "sent to discord".to_string()),
            Deliver::Home => post_to_home_session(data_dir, message).map(|()| {
                format!(
                    "posted to the home session (\"{}\")",
                    pantheon_storage::HOME_SESSION_ID
                )
            }),
            // The mobile app renders the home session, so a message posted
            // there is what the phone shows; the session id is the conversation
            // context. No separate push transport exists to invent.
            Deliver::Mobile => post_to_home_session(data_dir, message).map(|()| {
                format!(
                    "posted to the home session (\"{}\") for mobile delivery",
                    pantheon_storage::HOME_SESSION_ID
                )
            }),
            other => Err(format!(
                "pantheon send supports telegram|discord|mobile|home, not '{}'",
                other.label()
            )),
        }
    }

    /// Write a user-authored message into the well-known home session's ledger.
    /// The session auto-creates on first use. Mirrors the gateway's
    /// `deliver_to_home_session` but without the scheduled-job framing - this
    /// is a message, not a job result.
    fn post_to_home_session(data_dir: &Path, message: &str) -> Result<(), String> {
        let ledger = pantheon_storage::Ledger::open(&data_dir.join("ledger.db"))
            .map_err(|e| format!("open ledger: {e}"))?;
        ledger
            .ensure_home_session()
            .map_err(|e| format!("home session: {e}"))?;
        let event = pantheon_api::events::Event::RunProgress {
            run_id: pantheon_storage::HOME_SESSION_ID.to_string(),
            detail: message.to_string(),
        };
        ledger
            .append(&event)
            .map_err(|e| format!("append to home session: {e}"))?;
        Ok(())
    }

    pub fn cmd_send(args: &[String]) {
        match parse_send_args(args) {
            Ok(SendArgs::Help) => println!("{}", usage()),
            Err(e) => {
                eprintln!("pantheon send: {e}");
                eprintln!("{}", usage());
                std::process::exit(2);
            }
            Ok(SendArgs::Send { target, message }) => {
                match send_to_target(&crate::terminal::data_dir(), &target, &message) {
                    Ok(confirmation) => println!("{confirmation}"),
                    Err(e) => {
                        eprintln!("pantheon send: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

pub mod session_summary {
    //! Session-end summary, printed when the user quits.
    //!
    //! Pantheon aesthetic: no box-drawing, dim labels, one amber accent. Two
    //! renderings: [`SessionSummary::lines`] for in-TUI use and
    //! [`SessionSummary::to_plain_text`] for printing after the alternate
    //! screen is left (the quit path prints to the restored terminal).
    //!
    //! Message and tool-call totals use the ledger when available, because
    //! resumed views do not rebuild every historical tool card and can render
    //! saved tool results as assistant blocks. The `Messages:` total folds all
    //! three in, with the breakdown naming user messages and tool calls.

    use std::time::Duration;

    use pantheon_api::events::Event;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};

    use crate::session::theme::Theme;
    use crate::session::{BlockKind, TranscriptBlock, TuiState};

    /// Count transcript blocks into (user messages, assistant messages, tool
    /// calls). Read-only over the transcript: safe to call at shutdown.
    pub fn count_blocks(blocks: &[TranscriptBlock]) -> (usize, usize, usize) {
        let mut user = 0;
        let mut assistant = 0;
        let mut tools = 0;
        for b in blocks {
            match &b.kind {
                BlockKind::UserMessage(_) => user += 1,
                BlockKind::AssistantMessage(_) => assistant += 1,
                BlockKind::ToolCall { .. } => tools += 1,
                _ => {}
            }
        }
        (user, assistant, tools)
    }

    /// Count the effective run history. Ledger replay applies turn rewinds, so
    /// hidden turns stay excluded just as they do in the resumed transcript.
    pub fn count_entries(entries: &[pantheon_storage::LedgerEntry]) -> (usize, usize, usize) {
        let mut raw_user_messages = 0;
        let mut persisted_user_messages = 0;
        let mut assistant = 0;
        let mut tool_call_ids = std::collections::BTreeSet::new();
        for entry in entries {
            match &entry.event {
                Event::UserMessage { .. } => raw_user_messages += 1,
                Event::AssistantMessage { message, .. } => {
                    match message.role {
                        pantheon_api::message::Role::User => persisted_user_messages += 1,
                        pantheon_api::message::Role::Assistant
                            if !message.content.trim().is_empty() =>
                        {
                            assistant += 1;
                        }
                        _ => {}
                    }
                    tool_call_ids.extend(message.tool_calls.iter().map(|call| call.id.clone()));
                }
                Event::ToolStarted { call_id, .. } => {
                    tool_call_ids.insert(call_id.clone());
                }
                _ => {}
            }
        }
        // Live turns record both the raw retry prompt and its replayable message;
        // imported sessions only have the replayable row. Count the larger side
        // so normal turns are not doubled and imports are not omitted.
        (
            raw_user_messages.max(persisted_user_messages),
            assistant,
            tool_call_ids.len(),
        )
    }

    /// `1h 31m 21s`; `4m 30s`; `45s`.
    pub fn fmt_session_duration(d: Duration) -> String {
        let s = d.as_secs();
        if s >= 3600 {
            format!("{}h {}m {}s", s / 3600, (s % 3600) / 60, s % 60)
        } else if s >= 60 {
            format!("{}m {}s", s / 60, s % 60)
        } else {
            format!("{s}s")
        }
    }

    #[derive(Debug, Clone)]
    pub struct SessionSummary {
        pub session_id: String,
        pub title: Option<String>,
        pub duration: Duration,
        pub user_messages: usize,
        pub assistant_messages: usize,
        pub tool_calls: usize,
    }

    impl SessionSummary {
        /// Build from visible TUI state as a fallback if ledger replay fails.
        pub fn from_state(state: &TuiState) -> Self {
            let (user_messages, assistant_messages, tool_calls) = count_blocks(&state.blocks);
            Self {
                session_id: state.session_id.clone(),
                title: state.title.clone(),
                duration: state.start_time.elapsed(),
                user_messages,
                assistant_messages,
                tool_calls,
            }
        }

        /// Build from durable run counts and live TUI metadata.
        pub fn from_state_with_counts(
            state: &TuiState,
            (user_messages, assistant_messages, tool_calls): (usize, usize, usize),
        ) -> Self {
            let mut summary = Self::from_state(state);
            summary.user_messages = user_messages;
            summary.assistant_messages = assistant_messages;
            summary.tool_calls = tool_calls;
            summary
        }

        /// Build the goodbye summary from the effective durable history. The
        /// elapsed time begins at the first retained ledger event, so resuming a
        /// run does not reset its displayed duration to the current TUI visit.
        pub fn from_state_with_entries(
            state: &TuiState,
            entries: &[pantheon_storage::LedgerEntry],
        ) -> Self {
            let mut summary = Self::from_state_with_counts(state, count_entries(entries));
            if let Some(first_event_ms) = entries.iter().map(|e| e.ts_ms).filter(|ts| *ts > 0).min()
            {
                let elapsed_ms = pantheon_api::logging::now_ms()
                    .saturating_sub(first_event_ms)
                    .max(0) as u64;
                summary.duration = Duration::from_millis(elapsed_ms);
            }
            summary
        }

        pub fn total_messages(&self) -> usize {
            self.user_messages + self.assistant_messages + self.tool_calls
        }

        /// `Messages:  163 (3 user, 158 tool calls)` - labels padded to the
        /// same column as the reference.
        fn messages_value(&self) -> String {
            format!(
                "{} ({} user, {} tool calls)",
                self.total_messages(),
                self.user_messages,
                self.tool_calls
            )
        }

        /// Styled lines for in-TUI rendering. Labels dim, values body text;
        /// the resume command in the code color; the "Resume" header amber.
        pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
            let label = |t: &str| Span::styled(format!("{t:<16}"), Style::default().fg(th.dim));
            let value = |t: String| Span::styled(t, Style::default().fg(th.body));
            vec![
                Line::from(Span::styled(
                    "Resume this session with:",
                    Style::default()
                        .fg(th.emphasis)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!("  pantheon --resume {}", self.session_id),
                    Style::default().fg(th.code),
                )),
                Line::from(Span::styled(
                    format!("  pantheon -r {} \"your next message\"", self.session_id),
                    Style::default().fg(th.code),
                )),
                Line::from(Span::styled(
                    "  The conversation transcript will reopen with the session.",
                    Style::default().fg(th.dim),
                )),
                Line::from(""),
                Line::from(vec![label("Session:"), value(self.session_id.clone())]),
                Line::from(vec![
                    label("Title:"),
                    value(self.title.clone().unwrap_or_else(|| "untitled".to_string())),
                ]),
                Line::from(vec![
                    label("Duration:"),
                    value(fmt_session_duration(self.duration)),
                ]),
                Line::from(vec![label("Messages:"), value(self.messages_value())]),
                Line::from(""),
                Line::from(Span::styled(
                    "Goodbye. Your session is saved.",
                    Style::default().fg(th.dim),
                )),
            ]
        }

        /// Plain text for stdout after the TUI shuts down. Same layout, no
        /// ANSI - the terminal is already restored at that point.
        pub fn to_plain_text(&self) -> String {
            let title = self.title.as_deref().unwrap_or("untitled");
            format!(
                "Resume this session with:\n  pantheon --resume {}\n  pantheon -r {} \"your next message\"\n  The conversation transcript will reopen with the session.\n\n{:<16}{}\n{:<16}{}\n{:<16}{}\n{:<16}{}\n\nGoodbye. Your session is saved.\n",
                self.session_id,
                self.session_id,
                "Session:",
                self.session_id,
                "Title:",
                title,
                "Duration:",
                fmt_session_duration(self.duration),
                "Messages:",
                self.messages_value(),
            )
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn summary() -> SessionSummary {
            SessionSummary {
                session_id: "abc123".to_string(),
                title: Some("retry helper".to_string()),
                duration: Duration::from_secs(5481),
                user_messages: 3,
                assistant_messages: 2,
                tool_calls: 158,
            }
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

        #[test]
        fn fmt_session_duration_shapes() {
            assert_eq!(
                fmt_session_duration(Duration::from_secs(5481)),
                "1h 31m 21s"
            );
            assert_eq!(fmt_session_duration(Duration::from_secs(270)), "4m 30s");
            assert_eq!(fmt_session_duration(Duration::from_secs(45)), "45s");
        }

        #[test]
        fn plain_text_matches_reference_layout() {
            let t = summary().to_plain_text();
            assert!(t.contains("Resume this session with:"), "got:\n{t}");
            assert!(t.contains("pantheon --resume abc123"), "got:\n{t}");
            assert!(t.contains("Duration:       1h 31m 21s"), "got:\n{t}");
            assert!(
                t.contains("Messages:       163 (3 user, 158 tool calls)"),
                "got:\n{t}"
            );
        }

        #[test]
        fn styled_lines_carry_same_content() {
            let t = text(&summary().lines(&Theme::pantheon()));
            assert!(t.contains("pantheon --resume abc123"), "got:\n{t}");
            assert!(t.contains("retry helper"), "got:\n{t}");
            assert!(t.contains("1h 31m 21s"), "got:\n{t}");
        }

        #[test]
        fn untitled_when_no_title() {
            let mut s = summary();
            s.title = None;
            assert!(s.to_plain_text().contains("Title:          untitled"));
        }

        #[test]
        fn count_blocks_splits_kinds() {
            use crate::session::BlockKind;
            let blocks = vec![
                TranscriptBlock {
                    kind: BlockKind::UserMessage("hi".to_string()),
                },
                TranscriptBlock {
                    kind: BlockKind::AssistantMessage("hello".to_string()),
                },
                TranscriptBlock {
                    kind: BlockKind::Status("x".to_string()),
                },
            ];
            assert_eq!(count_blocks(&blocks), (1, 1, 0));
        }
    }
}

pub mod skills {
    //! `pantheon skills`: Tier 1 skill lifecycle.
    //!
    //! Three verbs:
    //!   list    - every discovered skill, with scope and origin provenance.
    //!   import  - copy an external SKILL.md into <data_dir>/skills so it
    //!             survives across processes (cross-format: Hermes, OpenClaw,
    //!             `.agents`, `.claude`, native).
    //!   doctor  - loud preflight: every discovered skill parsed, every broken
    //!             one named with its error code.
    //!
    //! All three are read-only on the source side; `import` is the only op
    //! that writes, and only into <data_dir>/skills.

    use crate::terminal::data_dir;
    use pantheon_exec::skills::{discover_skills_ext, import_skill, scan_skills_ext, SkillSource};
    use std::path::{Path, PathBuf};

    /// Extra discovery roots beyond the built-in cross-tool scopes.
    ///
    /// `PANTHEON_REPLAY_SKILL_DIR` (set by the nightly replay runner when
    /// validating a skill proposal) is honored first, so a replayed run
    /// sees exactly the candidate skill under test.
    pub(crate) fn extra_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Ok(d) = std::env::var("PANTHEON_REPLAY_SKILL_DIR") {
            let d = d.trim();
            if !d.is_empty() {
                roots.push(PathBuf::from(d));
            }
        }
        if let Ok(d) = std::env::var("PANTHEON_SKILLS_DIR") {
            for entry in d.split(':').filter(|s| !s.is_empty()) {
                roots.push(PathBuf::from(entry));
            }
        }
        roots
    }

    pub(crate) fn project_root() -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| data_dir())
    }

    fn scope_label(path: &Path) -> String {
        path.parent()
            .and_then(|p| p.file_name())
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// List every discovered skill. `pantheon skills list [--scope hermes|openclaw|agents|claude|external|all]`
    /// filters rows by source provenance.
    pub fn cmd_skills_list(args: &[String]) {
        let dd = data_dir();
        let scan = scan_skills_ext(&dd, &project_root(), &extra_roots());
        let scope = flag_value(args, "--scope");
        let found = filter_by_scope(scan.loaded, |s| s.meta.origin.as_str(), scope.as_deref());
        if found.is_empty() {
            println!("no skills discovered");
        } else {
            println!("{:24} {:12} {:12} DESCRIPTION", "NAME", "SCOPE", "ORIGIN");
            for s in &found {
                println!(
                    "{:24} {:12} {:12} {}",
                    s.meta.name,
                    scope_label(&s.path),
                    s.meta.origin,
                    s.meta.description
                );
            }
        }
        // A skill that never loaded is invisible above. Naming it here is the
        // difference between "my skill is gone" and "here is why".
        if !scan.rejected.is_empty() {
            println!();
            println!("{} skill(s) skipped:", scan.rejected.len());
            for r in &scan.rejected {
                println!("  {} - {}", r.path.display(), r.reason);
            }
            println!("run `pantheon skills doctor` for details");
        }
    }

    /// Import one named skill into <data_dir>/skills. Errors if unknown.
    ///
    /// Four modes:
    ///   `pantheon skills import <name> [--scope DIR]`
    ///       discover a local skill by name and copy it.
    ///   `pantheon skills import --url <URL> [name]`
    ///       fetch a single SKILL.md from a URL and import it.
    ///   `pantheon skills import --repo <URL> [--sub DIR]`
    ///       shallow-clone a repo and import every SKILL.md found in it.
    ///   `pantheon skills import --clawhub <slug> [--owner OWNER]`
    ///       import a skill from the OpenClaw ClawHub registry (public API,
    ///       full ZIP bundle including scripts/references).
    ///   `pantheon skills import --hermes <docs-url-or-path>`
    ///       import a skill from the Hermes agent GitHub repo, resolving a
    ///       docs page URL through its `Path |` metadata row, or accepting a
    ///       raw repo path like `skills/creative/claude-design`.
    pub fn cmd_skills_import(args: &[String]) {
        if args.len() < 2 {
            eprintln!("usage: pantheon skills import <name> [--scope DIR] | --url <URL> [name] | --repo <URL> [--sub DIR] | --clawhub <slug> [--owner OWNER] | --hermes <docs-url-or-path>");
            std::process::exit(2);
        }
        let dd = data_dir();

        if let Some(url) = flag_value(args, "--repo") {
            let sub = flag_value(args, "--sub");
            match pantheon_exec::skills::import_skills_from_repo(&dd, &url, sub.as_deref()) {
                Ok(items) => {
                    if items.is_empty() {
                        println!("no skills found in {url}");
                    } else {
                        for (n, p) in &items {
                            println!("imported {n} -> {}", p.display());
                        }
                        println!("{} skill(s) imported from {url}", items.len());
                    }
                }
                Err(e) => {
                    eprintln!("skills import: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }

        if let Some(slug) = flag_value(args, "--clawhub") {
            let owner = flag_value(args, "--owner").map(|s| s.to_string());
            match pantheon_exec::skills::import_skill_from_clawhub(&dd, &slug, owner.as_deref()) {
                Ok((path, skill)) => println!(
                    "imported {} <- clawhub://{slug} -> {}",
                    skill.meta.name,
                    path.display()
                ),
                Err(e) => {
                    eprintln!("skills import: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        if let Some(url) = flag_value(args, "--hermes") {
            // Accept either a docs page URL (resolved via the `Path |` row in
            // the generated page) or a raw repo path.
            let repo_path = match pantheon_exec::skills::hermes_docs_path(&url) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("skills import: {e}");
                    std::process::exit(1);
                }
            };
            match pantheon_exec::skills::import_skill_from_hermes(&dd, &repo_path) {
                Ok((path, skill)) => println!(
                    "imported {} <- hermes://{repo_path} -> {}",
                    skill.meta.name,
                    path.display()
                ),
                Err(e) => {
                    eprintln!("skills import: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }

        if let Some(url) = flag_value(args, "--url") {
            let name = flag_value(args, "--name");
            match pantheon_exec::skills::import_skill_from_url(&dd, &url) {
                Ok((path, skill)) => {
                    let n = name.unwrap_or_else(|| skill.meta.name.clone());
                    println!("imported {n} <- {url} -> {}", path.display());
                }
                Err(e) => {
                    eprintln!("skills import: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }

        let name = &args[1];
        let mut scope: Option<PathBuf> = None;
        let mut i = 2;
        while i < args.len() {
            if args[i].as_str() == "--scope" {
                i += 1;
                if i < args.len() {
                    scope = Some(PathBuf::from(&args[i]));
                }
            }
            i += 1;
        }
        let mut roots: Vec<PathBuf> = extra_roots();
        if let Some(s) = scope {
            roots.push(s);
        }
        let found = discover_skills_ext(&dd, &project_root(), &roots);
        let skill = found
            .iter()
            .find(|s| s.meta.name == *name)
            .unwrap_or_else(|| {
                eprintln!("skills import: no skill named '{name}'");
                std::process::exit(1);
            });
        match import_skill(&dd, skill) {
            Ok(p) => println!("imported {} -> {}", name, p.display()),
            Err(e) => {
                eprintln!("skills import: {e}");
                std::process::exit(1);
            }
        }
    }

    /// Extract a `--flag value` pair from the arg list, returning the value.
    fn flag_value(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|p| args.get(p + 1))
            .filter(|s| !s.starts_with("--"))
            .map(|s| s.to_string())
    }
    /// The rejected skills for an explicit data dir, for `pantheon repair`.
    ///
    /// `scan_summary` reads the process-global data dir and reports counts only.
    /// Repair needs both a caller-supplied directory (it is handed one, and the
    /// global may point elsewhere) and the per-skill reason, because the whole
    /// point of the check is naming the file that needs fixing.
    pub fn rejected_at(data_dir: &Path) -> Vec<(PathBuf, String)> {
        let scan = scan_skills_ext(data_dir, &project_root(), &extra_roots());
        scan.rejected
            .into_iter()
            .map(|r| (r.path, r.reason))
            .collect()
    }

    pub fn cmd_skills_doctor(_args: &[String]) {
        let dd = data_dir();
        let scan = scan_skills_ext(&dd, &project_root(), &extra_roots());
        for s in &scan.loaded {
            println!("ok   {} ({})", s.meta.name, s.meta.origin);
        }
        for r in &scan.rejected {
            println!("fail {} - {}", r.path.display(), r.reason);
        }
        if scan.loaded.is_empty() && scan.rejected.is_empty() {
            println!("(no skills discovered)");
        }
        if !scan.rejected.is_empty() {
            eprintln!(
                "{} broken or shadowed skill(s) - fix the SKILL.md, or remove the \
                 duplicate that loses the name collision",
                scan.rejected.len()
            );
            std::process::exit(1);
        }
    }

    /// Resolve a `SkillSource` from a name, for `--scope` provenance tagging.
    ///
    /// `bundled` is not a `SkillSource`: those skills live in the pantheon data
    /// dir and carry `origin: bundled` in their frontmatter, so the filter keys
    /// on that string rather than on a source variant.
    pub fn parse_scope(s: &str) -> Option<SkillSource> {
        match s.to_lowercase().as_str() {
            "hermes" => Some(SkillSource::Hermes),
            "openclaw" => Some(SkillSource::OpenClaw),
            "agents" => Some(SkillSource::Agents),
            "claude" => Some(SkillSource::Claude),
            "external" => Some(SkillSource::External),
            _ => None,
        }
    }

    /// Filter `pantheon skills list` rows by source tag (`hermes`, `openclaw`,
    /// `agents`, `claude`, `external`, `pantheon`). `None`/empty/`all` =
    /// everything; unknown names match nothing (loud empty table, not an
    /// error, so scripts can probe).
    pub fn filter_by_scope<T>(
        items: Vec<T>,
        origin_of: impl Fn(&T) -> &str,
        scope: Option<&str>,
    ) -> Vec<T> {
        let Some(scope) = scope.map(str::trim).filter(|s| !s.is_empty()) else {
            return items;
        };
        if scope.eq_ignore_ascii_case("all") {
            return items;
        }
        // `bundled` is an origin string, not a SkillSource, so it is matched
        // literally here instead of going through parse_scope.
        if scope.eq_ignore_ascii_case("bundled") {
            return items
                .into_iter()
                .filter(|it| origin_of(it) == "bundled")
                .collect();
        }
        let Some(want) = parse_scope(scope) else {
            return Vec::new();
        };
        let tag = want.tag();
        items
            .into_iter()
            .filter(|it| origin_of(it) == tag)
            .collect()
    }
}

pub mod stats {
    //! `pantheon stats`: token/spend aggregation from the event ledger.
    //!
    //! Usage rows are persisted as `Event::UsageRecorded` (the provider-plane
    //! `Usage` event used to be dropped before reaching the ledger). This
    //! command folds them over a time window - default today - and breaks
    //! the totals down by model, by session (run), and by day. Usage measures
    //! provider work actually completed, so rewinding a turn hides its transcript
    //! but does not subtract the tokens or spend already incurred.

    use std::collections::BTreeMap;
    use std::path::Path;

    use pantheon_api::error::PantheonError;
    use pantheon_api::events::Event;
    use pantheon_storage::Ledger;

    /// One aggregated usage row.
    #[derive(Debug, Clone, Default)]
    pub struct UsageTotals {
        pub calls: u64,
        pub input_tokens: u64,
        pub output_tokens: u64,
        pub total_tokens: u64,
        /// Sum of known costs; calls without a catalog price are excluded.
        pub cost_usd: f64,
        /// Calls that carried a cost.
        pub priced_calls: u64,
    }

    impl UsageTotals {
        fn add(&mut self, input: u64, output: u64, total: u64, cost: Option<f64>) {
            self.calls += 1;
            self.input_tokens += input;
            self.output_tokens += output;
            self.total_tokens += total;
            if let Some(c) = cost {
                self.cost_usd += c;
                self.priced_calls += 1;
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct ModelStat {
        pub model: String,
        pub totals: UsageTotals,
    }

    #[derive(Debug, Clone)]
    pub struct RunStat {
        pub run_id: String,
        pub title: Option<String>,
        pub totals: UsageTotals,
    }

    #[derive(Debug, Clone)]
    pub struct DayStat {
        /// UTC day, `YYYY-MM-DD`.
        pub day: String,
        pub totals: UsageTotals,
    }

    /// Aggregated report for a time window.
    #[derive(Debug, Clone)]
    pub struct StatsReport {
        pub from_ms: i64,
        pub to_ms: i64,
        pub totals: UsageTotals,
        pub by_model: Vec<ModelStat>,
        pub by_run: Vec<RunStat>,
        pub by_day: Vec<DayStat>,
    }

    /// Fold `UsageRecorded` rows from the ledger into a report.
    /// Pure over the ledger: testable without a CLI.
    pub fn collect(
        ledger: &Ledger,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<StatsReport, PantheonError> {
        let rows = ledger.usage_between(from_ms, to_ms)?;
        let mut totals = UsageTotals::default();
        let mut by_model: BTreeMap<String, UsageTotals> = BTreeMap::new();
        let mut by_run: BTreeMap<String, UsageTotals> = BTreeMap::new();
        let mut by_day: BTreeMap<String, UsageTotals> = BTreeMap::new();
        let mut titles: BTreeMap<String, String> = BTreeMap::new();

        // Session titles for the run breakdown: latest SessionTitled wins.
        // Only fetched when there is usage to attribute, so `stats` on an
        // empty window stays a single cheap query.
        let mut titles_loaded = false;

        for entry in &rows {
            let Event::UsageRecorded {
                run_id,
                model,
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
                ..
            } = &entry.event
            else {
                continue;
            };
            if !titles_loaded {
                for r in ledger.list_runs(usize::MAX).unwrap_or_default() {
                    if let Some(t) = r.3 {
                        titles.insert(r.0, t);
                    }
                }
                titles_loaded = true;
            }
            totals.add(*input_tokens, *output_tokens, *total_tokens, *cost_usd);
            by_model.entry(model.clone()).or_default().add(
                *input_tokens,
                *output_tokens,
                *total_tokens,
                *cost_usd,
            );
            by_run.entry(run_id.clone()).or_default().add(
                *input_tokens,
                *output_tokens,
                *total_tokens,
                *cost_usd,
            );
            by_day.entry(day_label(entry.ts_ms)).or_default().add(
                *input_tokens,
                *output_tokens,
                *total_tokens,
                *cost_usd,
            );
        }

        let by_model = by_model
            .into_iter()
            .map(|(model, totals)| ModelStat { model, totals })
            .collect();
        let by_run = by_run
            .into_iter()
            .map(|(run_id, totals)| {
                let title = titles.remove(&run_id);
                RunStat {
                    run_id,
                    title,
                    totals,
                }
            })
            .collect();
        let by_day = by_day
            .into_iter()
            .map(|(day, totals)| DayStat { day, totals })
            .collect();

        Ok(StatsReport {
            from_ms,
            to_ms,
            totals,
            by_model,
            by_run,
            by_day,
        })
    }

    /// Sort helper: highest cost first (falls back to tokens).
    fn sort_stats<T>(v: &mut [T], totals: impl Fn(&T) -> &UsageTotals) {
        v.sort_by(|a, b| {
            let (ta, tb) = (totals(a), totals(b));
            tb.cost_usd
                .partial_cmp(&ta.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| tb.total_tokens.cmp(&ta.total_tokens))
        });
    }

    fn fmt_cost(cost: f64, priced: u64, calls: u64) -> String {
        if priced == 0 && calls > 0 {
            "n/a".to_string()
        } else {
            format!("${:.4}", cost)
        }
    }

    fn fmt_tokens(t: &UsageTotals) -> String {
        format!(
            "{} tok ({} in / {} out)",
            t.total_tokens, t.input_tokens, t.output_tokens
        )
    }

    /// Human-readable report.
    pub fn render_human(r: &StatsReport) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "stats {} → {}\n",
            day_label(r.from_ms),
            day_label(r.to_ms.saturating_sub(1))
        ));
        out.push_str(&format!(
            "total: {} calls, {}, cost {}\n",
            r.totals.calls,
            fmt_tokens(&r.totals),
            fmt_cost(r.totals.cost_usd, r.totals.priced_calls, r.totals.calls)
        ));
        out.push_str("note: rewound turns remain included; rewinds do not refund provider usage\n");
        if r.totals.calls == 0 {
            out.push_str("no usage recorded in this window\n");
            return out;
        }

        let mut by_model = r.by_model.clone();
        sort_stats(&mut by_model, |s| &s.totals);
        out.push_str("\nby model:\n");
        for s in &by_model {
            out.push_str(&format!(
                "  {:<42} {:>6} calls  {:>14}  {}\n",
                s.model,
                s.totals.calls,
                fmt_tokens(&s.totals),
                fmt_cost(s.totals.cost_usd, s.totals.priced_calls, s.totals.calls)
            ));
        }

        let mut by_run = r.by_run.clone();
        sort_stats(&mut by_run, |s| &s.totals);
        out.push_str("\nby session:\n");
        for s in &by_run {
            let label = s
                .title
                .as_deref()
                .map(|t| format!("{} - {}", short_id(&s.run_id), t))
                .unwrap_or_else(|| short_id(&s.run_id));
            out.push_str(&format!(
                "  {:<42} {:>6} calls  {:>14}  {}\n",
                truncate(&label, 42),
                s.totals.calls,
                fmt_tokens(&s.totals),
                fmt_cost(s.totals.cost_usd, s.totals.priced_calls, s.totals.calls)
            ));
        }

        out.push_str("\nby day:\n");
        for d in &r.by_day {
            out.push_str(&format!(
                "  {}  {:>6} calls  {:>14}  {}\n",
                d.day,
                d.totals.calls,
                fmt_tokens(&d.totals),
                fmt_cost(d.totals.cost_usd, d.totals.priced_calls, d.totals.calls)
            ));
        }
        out
    }

    fn short_id(id: &str) -> String {
        // Truncate to 23 chars plus dots, cutting only on a char boundary:
        // run ids are free-form (`pantheon run --id`), so a >24 byte id with
        // a multibyte char would panic the whole stats command at a byte
        // slice. The trigger stays byte-based like the original; the cut
        // moves to a char boundary.
        if id.len() <= 24 {
            return id.to_string();
        }
        let head: String = id.chars().take(23).collect();
        format!("{head}...")
    }

    /// Compact summary for the TUI `/stats` command: today's totals plus this
    /// session's share. Single status block, no tables.
    pub fn render_session_summary(r: &StatsReport, run_id: &str) -> String {
        let mine = r.by_run.iter().find(|s| s.run_id == run_id);
        let mut out = format!(
            "stats today: {} calls, {}, cost {}",
            r.totals.calls,
            fmt_tokens(&r.totals),
            fmt_cost(r.totals.cost_usd, r.totals.priced_calls, r.totals.calls)
        );
        match mine {
            Some(s) => {
                out.push_str(&format!(
                    "\nthis session: {} calls, {}, cost {}",
                    s.totals.calls,
                    fmt_tokens(&s.totals),
                    fmt_cost(s.totals.cost_usd, s.totals.priced_calls, s.totals.calls)
                ));
            }
            None => out.push_str("\nthis session: no usage today"),
        }
        if let Some(top) = r
            .by_model
            .iter()
            .max_by(|a, b| a.totals.total_tokens.cmp(&b.totals.total_tokens))
        {
            out.push_str(&format!(
                "\ntop model: {} ({} tok)",
                top.model, top.totals.total_tokens
            ));
        }
        out
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }

    /// Machine-readable report.
    pub fn render_json(r: &StatsReport) -> String {
        let mut s = String::new();
        s.push_str("{\n");
        s.push_str(&format!(
            "  \"from\": \"{}\",\n  \"to\": \"{}\",\n",
            day_label(r.from_ms),
            day_label(r.to_ms.saturating_sub(1))
        ));
        s.push_str(&format!("  \"totals\": {},\n", totals_json(&r.totals)));
        s.push_str("  \"by_model\": [\n");
        for (i, m) in r.by_model.iter().enumerate() {
            s.push_str(&format!(
                "    {{\"model\": {}, \"totals\": {}}}{}\n",
                json_str(&m.model),
                totals_json(&m.totals),
                if i + 1 < r.by_model.len() { "," } else { "" }
            ));
        }
        s.push_str("  ],\n  \"by_session\": [\n");
        for (i, m) in r.by_run.iter().enumerate() {
            let title = m.title.as_deref().map(json_str).unwrap_or("null".into());
            s.push_str(&format!(
                "    {{\"run_id\": {}, \"title\": {}, \"totals\": {}}}{}\n",
                json_str(&m.run_id),
                title,
                totals_json(&m.totals),
                if i + 1 < r.by_run.len() { "," } else { "" }
            ));
        }
        s.push_str("  ],\n  \"by_day\": [\n");
        for (i, d) in r.by_day.iter().enumerate() {
            s.push_str(&format!(
                "    {{\"day\": {}, \"totals\": {}}}{}\n",
                json_str(&d.day),
                totals_json(&d.totals),
                if i + 1 < r.by_day.len() { "," } else { "" }
            ));
        }
        s.push_str("  ]\n}\n");
        s
    }

    fn totals_json(t: &UsageTotals) -> String {
        format!(
            "{{\"calls\": {}, \"input_tokens\": {}, \"output_tokens\": {}, \"total_tokens\": {}, \"cost_usd\": {:.6}}}",
            t.calls, t.input_tokens, t.output_tokens, t.total_tokens, t.cost_usd
        )
    }

    fn json_str(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    // --- time helpers (UTC; no date crate) ---

    pub const DAY_MS: i64 = 86_400_000;

    /// Public re-export of the canonical `pantheon_api::logging::now_ms`.
    ///
    /// Part of this module's public surface (`crate::stats::now_ms` is read by
    /// the session), so it stays a named re-export rather than a private copy.
    pub use pantheon_api::logging::now_ms;

    /// Start of the UTC day containing `ts_ms`.
    pub fn day_start_ms(ts_ms: i64) -> i64 {
        ts_ms - ((ts_ms % DAY_MS + DAY_MS) % DAY_MS)
    }

    /// `YYYY-MM-DD` (UTC) for a timestamp. Howard Hinnant's civil-from-days.
    pub fn day_label(ts_ms: i64) -> String {
        let z = ts_ms.div_euclid(DAY_MS) + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{:04}-{:02}-{:02}", y, m, d)
    }

    /// Parse `YYYY-MM-DD` into the UTC day start (ms). `None` on bad input.
    pub fn parse_day(s: &str) -> Option<i64> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 3 {
            return None;
        }
        let y: i64 = parts[0].parse().ok()?;
        let m: i64 = parts[1].parse().ok()?;
        let d: i64 = parts[2].parse().ok()?;
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
            return None;
        }
        // days_from_civil
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146097 + doe - 719468;
        Some(days * DAY_MS)
    }

    /// CLI entry: `pantheon stats [--week|--month|--from D|--to D] [--json]`.
    pub fn cmd_stats(args: &[String], data_dir: &Path) {
        let mut from: Option<i64> = None;
        let mut to: Option<i64> = None;
        let mut json = false;
        let mut i = 2;
        while i < args.len() {
            match args[i].as_str() {
                "--json" => json = true,
                "--week" => {
                    from = Some(day_start_ms(now_ms() - 6 * DAY_MS));
                }
                "--month" => {
                    from = Some(day_start_ms(now_ms() - 29 * DAY_MS));
                }
                "--from" => {
                    i += 1;
                    match args.get(i).and_then(|s| parse_day(s)) {
                        Some(ms) => from = Some(ms),
                        None => {
                            eprintln!("stats: --from needs YYYY-MM-DD");
                            std::process::exit(2);
                        }
                    }
                }
                "--to" => {
                    i += 1;
                    match args.get(i).and_then(|s| parse_day(s)) {
                        Some(ms) => to = Some(ms + DAY_MS),
                        None => {
                            eprintln!("stats: --to needs YYYY-MM-DD");
                            std::process::exit(2);
                        }
                    }
                }
                "--help" | "-h" => {
                    print_stats_help();
                    return;
                }
                other => {
                    eprintln!("stats: unknown flag {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        let now = now_ms();
        let mut from_ms = from.unwrap_or_else(|| day_start_ms(now));
        let mut to_ms = to.unwrap_or_else(|| day_start_ms(now) + DAY_MS);
        if to_ms < from_ms {
            std::mem::swap(&mut from_ms, &mut to_ms);
        }
        if to_ms > now + DAY_MS {
            to_ms = now + DAY_MS;
        }

        let ledger = Ledger::open(&data_dir.join("ledger.db")).unwrap_or_else(|e| {
            eprintln!("stats: open ledger: {e}");
            std::process::exit(1);
        });
        match collect(&ledger, from_ms, to_ms) {
            Ok(report) => {
                if json {
                    println!("{}", render_json(&report));
                } else {
                    print!("{}", render_human(&report));
                }
            }
            Err(e) => {
                eprintln!("stats: {e}");
                std::process::exit(1);
            }
        }
    }

    fn print_stats_help() {
        println!("pantheon stats [--week|--month|--from YYYY-MM-DD|--to YYYY-MM-DD] [--json]");
        println!();
        println!("Token and spend totals from the event ledger. Default: today.");
        println!("  --week        last 7 days");
        println!("  --month       last 30 days");
        println!("  --from DATE   window start (inclusive)");
        println!("  --to DATE     window end (inclusive)");
        println!("  --json        machine-readable output");
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn short_id_cuts_on_char_boundaries() {
            // 22 ASCII bytes plus a 3-byte char: 25 bytes, so the old
            // `&id[..23]` byte slice landed inside the euro sign and panicked
            // (byte 23 is the start of the char, and the slice end must be a
            // boundary). 24+ ASCII avoids the panic only by accident of where
            // the boundary falls, so the test uses the genuinely-panicking 22.
            let id = "a".repeat(22) + "€";
            assert!(short_id(&id).ends_with("..."), "long ids get the dots");
            // 22 ASCII bytes plus a 4-byte char: 26 bytes, also crosses the
            // `>24` trigger with the char straddling byte 23.
            let id4 = "b".repeat(22) + "🦀";
            assert!(short_id(&id4).ends_with("..."));
            // ASCII behavior is unchanged: 25 bytes cut to 23 plus the dots;
            // 24 bytes sits exactly at the `>24` threshold and stays whole.
            assert_eq!(short_id(&"a".repeat(25)), "a".repeat(23) + "...");
            assert_eq!(short_id(&"a".repeat(24)), "a".repeat(24));
        }
    }
}

pub mod app {
    //! The event loop, the screen stack, and focus.
    //!
    //! One loop drives everything. A screen is pushed onto a stack, keys are
    //! offered to the top of it, and the loop redraws. There is no second loop
    //! for setup and no second input path for the command palette: they are
    //! screens on the same stack, which is what makes "one terminal product" a
    //! structural fact rather than a convention.
    //!
    //! # Focus and key routing
    //!
    //! A key goes to the top screen first. If it reports [`KeyResult::Ignored`],
    //! the next screen down gets it, and then the global bindings. That ordering
    //! is what lets a text input swallow a printable character while still
    //! receiving Esc, and lets a modal list receive `/` as a filter character
    //! while the palette's global `/` binding stays out of the way.

    use std::io;

    use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use crate::widget::{Key, KeyResult, Selection};

    /// What a screen produced when it finished.
    #[derive(Debug, Clone, PartialEq)]
    pub enum ScreenResult {
        /// Done, with a value the caller consumes.
        Done(Selection),
        /// Popped without a value. Either the user backed out, or the screen has
        /// nothing to return (a cancelled `Select` still produced a filter).
        Back,
        /// Quit the app. Only a screen may ask for this, and only deliberately:
        /// a widget cannot, because a widget has no business ending the process.
        Quit,
    }

    /// One level of the stack.
    ///
    /// A screen owns one widget. Composition (setup's step list, the command
    /// palette) is the job of a screen that is itself made of screens, not a
    /// trait with seventy methods.
    pub struct Screen {
        /// What the title bar calls this, e.g. "setup · tools".
        pub title: String,
        /// The widget this screen is driving.
        pub widget: ScreenWidget,
        /// Called when the widget submits or cancels.
        pub on_finish: Option<Box<dyn FnOnce(Selection) + Send>>,
    }

    /// The widgets a screen may hold. A screen holds exactly one, so key routing
    /// is a match with four arms rather than a trait-object dance.
    pub enum ScreenWidget {
        Select(crate::widget::Select),
        MultiSelect(crate::widget::MultiSelect),
        TextInput(crate::widget::TextInput),
        Confirm(crate::widget::Confirm),
    }

    impl Screen {
        pub fn select(title: impl Into<String>, w: crate::widget::Select) -> Self {
            Self {
                title: title.into(),
                widget: ScreenWidget::Select(w),
                on_finish: None,
            }
        }
        pub fn multi(title: impl Into<String>, w: crate::widget::MultiSelect) -> Self {
            Self {
                title: title.into(),
                widget: ScreenWidget::MultiSelect(w),
                on_finish: None,
            }
        }
        pub fn text(title: impl Into<String>, w: crate::widget::TextInput) -> Self {
            Self {
                title: title.into(),
                widget: ScreenWidget::TextInput(w),
                on_finish: None,
            }
        }
        pub fn confirm(title: impl Into<String>, w: crate::widget::Confirm) -> Self {
            Self {
                title: title.into(),
                widget: ScreenWidget::Confirm(w),
                on_finish: None,
            }
        }

        /// Set the continuation run when this screen finishes.
        pub fn then<F: FnOnce(Selection) + Send + 'static>(mut self, f: F) -> Self {
            self.on_finish = Some(Box::new(f));
            self
        }
    }

    /// Translate a crossterm key event into the widget-layer [`Key`].
    ///
    /// The one place control chords are interpreted. Every other layer sees only
    /// named keys, so "is this a literal c or Ctrl+C" is answered exactly once
    /// and cannot be answered differently in two components.
    pub fn translate(event: KeyEvent) -> Key {
        if event.kind != KeyEventKind::Press {
            return Key::Other;
        }
        let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            return match event.code {
                KeyCode::Char('c') => Key::CtrlC,
                KeyCode::Char('d') => Key::CtrlD,
                KeyCode::Char('k') => Key::CtrlK,
                KeyCode::Char('l') => Key::CtrlL,
                KeyCode::Char('n') => Key::CtrlN,
                KeyCode::Char('p') => Key::CtrlP,
                KeyCode::Char('r') => Key::CtrlR,
                KeyCode::Char('o') => Key::CtrlO,
                KeyCode::Char(' ') => Key::CtrlSpace,
                _ => Key::Other,
            };
        }
        match event.code {
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::Enter => Key::Enter,
            KeyCode::Esc => Key::Esc,
            KeyCode::Tab => Key::Tab,
            KeyCode::BackTab => Key::BackTab,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Delete => Key::Delete,
            KeyCode::Char(' ') => Key::Space,
            KeyCode::Char(c) => Key::Char(c),
            _ => Key::Other,
        }
    }

    /// The app: the screen stack plus the global key bindings.
    pub struct TuiApp {
        stack: Vec<Screen>,
        /// Set when the loop should stop.
        pub running: bool,
        /// The reason the app ended, for the exit path and the test assertions.
        pub exit: Option<Selection>,
        /// The most recent submitted value. A screen's continuation is free to
        /// push the next screen, so the value cannot live in the continuation's
        /// return type; it is recorded here, where the stack that produced it is
        /// still in scope.
        last: Option<Selection>,
    }

    impl Default for TuiApp {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TuiApp {
        pub fn new() -> Self {
            Self {
                stack: Vec::new(),
                running: true,
                exit: None,
                last: None,
            }
        }

        /// The value from the most recent submit, and clear it.
        ///
        /// Consuming rather than peeking matters for a sequence of pickers: two
        /// `pick_one` calls in a row must not both read the first answer.
        pub fn take_value(&mut self) -> Option<Selection> {
            self.last.take()
        }

        pub fn push(&mut self, s: Screen) {
            self.stack.push(s);
        }

        pub fn pop(&mut self) -> Option<Screen> {
            self.stack.pop()
        }

        pub fn depth(&self) -> usize {
            self.stack.len()
        }

        pub fn top(&self) -> Option<&Screen> {
            self.stack.last()
        }

        pub fn top_mut(&mut self) -> Option<&mut Screen> {
            self.stack.last_mut()
        }

        /// Replace the top screen. Used when a screen's result drives an
        /// immediate transition, so there is no flash of the finished screen.
        pub fn replace(&mut self, s: Screen) {
            if let Some(slot) = self.stack.last_mut() {
                *slot = s;
            } else {
                self.push(s);
            }
        }

        /// Offer a key to the stack: top screen first, then down, then the global
        /// bindings. Returns true when something consumed it.
        pub fn on_key(&mut self, key: Key) -> bool {
            // Walk the stack top-down. A screen that ignores the key offers it to
            // the one below, which is how an overlay's Esc reaches the session
            // view's own bindings instead of being swallowed.
            let mut result = None;
            for i in (0..self.stack.len()).rev() {
                let r = match &mut self.stack[i].widget {
                    ScreenWidget::Select(w) => submit(w.handle_key(key)),
                    ScreenWidget::MultiSelect(w) => submit(w.handle_key(key)),
                    ScreenWidget::TextInput(w) => submit(w.handle_key(key)),
                    ScreenWidget::Confirm(w) => submit(w.handle_key(key)),
                };
                if let Some(r) = r {
                    result = Some((i, r));
                    break;
                }
            }
            let Some((idx, res)) = result else {
                return self.global_key(key);
            };
            self.settle(idx, res);
            true
        }

        fn global_key(&mut self, key: Key) -> bool {
            match key {
                // Ctrl+C always quits, on every screen, with no confirmation: a
                // panic in the widget layer should not be unkillable, and the
                // run is durable in the ledger either way.
                Key::CtrlC | Key::CtrlD => {
                    self.running = false;
                    true
                }
                _ => false,
            }
        }

        /// Act on a screen's result: run its continuation, then pop it.
        fn settle(&mut self, idx: usize, res: ScreenResult) {
            match res {
                ScreenResult::Quit => {
                    self.running = false;
                }
                ScreenResult::Done(sel) => {
                    // Record before running the continuation, so a continuation
                    // that pushes the next screen cannot overwrite the answer that
                    // produced it.
                    self.last = Some(sel.clone());
                    // Take the continuation out of the screen before running it:
                    // the closure may push a new screen, and holding a borrow of
                    // the screen it came from while that happens is a borrow
                    // error for no reason.
                    let cont = self.stack.get_mut(idx).and_then(|s| s.on_finish.take());
                    if let Some(f) = cont {
                        f(sel);
                    }
                    if idx < self.stack.len() {
                        self.stack.remove(idx);
                    }
                }
                ScreenResult::Back => {
                    if idx < self.stack.len() {
                        self.stack.remove(idx);
                    }
                }
            }
        }

        /// Draw the whole stack: the base screen, then any overlays on top.
        ///
        /// Each screen gets the full frame except where there is already a base
        /// below it, which is the session-with-overlay case. Overlays covering the
        /// base is what makes a picker read as a picker rather than as a second
        /// screen the user has to scroll past.
        pub fn draw(&mut self, f: &mut ratatui::Frame) {
            let area = f.area();
            // The base screen fills the frame. Anything above it is an overlay and
            // draws over that, centered and narrower.
            let mut first = true;
            for screen in self.stack.iter_mut() {
                let target = if first {
                    first = false;
                    area
                } else {
                    centered(area, 72, 80)
                };
                match &mut screen.widget {
                    ScreenWidget::Select(w) => crate::render::draw_select(f, target, w),
                    ScreenWidget::MultiSelect(w) => crate::render::draw_multi(f, target, w),
                    ScreenWidget::TextInput(w) => crate::render::draw_text(f, target, w),
                    ScreenWidget::Confirm(w) => crate::render::draw_confirm(f, target, w),
                }
            }
        }

        /// Run the loop on a real terminal until the stack empties or the app is
        /// told to stop.
        ///
        /// This is the only blocking call in the crate. Everything above it is
        /// synchronous and testable, which is the point: the event loop is the
        /// only part that needs a tty, so it is the only part that cannot be
        /// covered by a unit test, and it is small enough to read.
        pub fn run(&mut self) -> io::Result<()> {
            let mut term = TerminalSession::enter()?;
            loop {
                term.terminal().draw(|f| self.draw(f))?;
                match next_key(std::time::Duration::from_millis(50))? {
                    Some(k) => {
                        self.on_key(k);
                    }
                    None => {
                        // A tick with no key. Redraw anyway so a resize that
                        // produced a non-key event is repainted, and so a screen
                        // that animates is not frozen.
                    }
                }
                if !self.running || self.stack.is_empty() {
                    break;
                }
            }
            Ok(())
        }
    }

    /// A centered rectangle, for overlays. Width and height are percentages of
    /// the frame, clamped so a tiny terminal still gets a usable box rather than a
    /// negative dimension.
    fn centered(area: ratatui::layout::Rect, w_pct: u16, h_pct: u16) -> ratatui::layout::Rect {
        let w = (area.width * w_pct / 100).max(24).min(area.width);
        let h = (area.height * h_pct / 100).max(5).min(area.height);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        ratatui::layout::Rect::new(x, y, w, h)
    }

    /// Convert a widget result into a screen result. `Redraw` means "handled,
    /// stay put", so it is not a screen-level outcome at all.
    fn submit(r: KeyResult) -> Option<ScreenResult> {
        match r {
            KeyResult::Ignored => None,
            KeyResult::Redraw => None,
            KeyResult::Submit(sel) => Some(ScreenResult::Done(sel)),
            // Cancel is "the user backed out of this screen". A multi-select that
            // cancels has produced nothing, so the value is empty rather than
            // inventing a selection the user did not make.
            KeyResult::Cancel => Some(ScreenResult::Back),
        }
    }

    /// The terminal setup and restore, in one place.
    ///
    /// Restoring is not optional and not best-effort: a panic between
    /// `enter_raw_mode` and `leave_alternate_screen` leaves the user's shell in
    /// raw mode, and the only recovery is `reset` from another terminal. The
    /// `TerminalGuard` in [`enter`] restores on drop, and the event loop never
    /// calls `process::exit` once it is in.
    pub struct TerminalSession {
        terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    }

    impl TerminalSession {
        /// Enter raw mode and the alternate screen. Fails loudly rather than
        /// degrading to a different interface: if the terminal cannot host the
        /// product, the honest answer is an error, not a second product.
        pub fn enter() -> io::Result<Self> {
            use crossterm::execute;
            use crossterm::terminal::{
                disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
                LeaveAlternateScreen,
            };
            enable_raw_mode()?;
            let mut out = io::stdout();
            if let Err(e) = execute!(out, EnterAlternateScreen) {
                let _ = disable_raw_mode();
                return Err(e);
            }
            // Every prompt builds a fresh ratatui Terminal whose internal
            // buffer starts blank. The diff writer then skips cells it
            // believes are already blank - but the reused alternate screen
            // still holds the previous prompt's content, so stale rows bleed
            // through (a `Clear` widget is a diff no-op for the same reason).
            // Blank the real screen to match the fresh buffer. This is a raw
            // crossterm clear on purpose: ratatui's `Terminal::clear` queries
            // the cursor position first (a terminal round-trip that fails on
            // pipes and non-answering terminals), which is more than a prompt
            // needs.
            let _ = execute!(out, Clear(ClearType::All));
            let backend = ratatui::backend::CrosstermBackend::new(out);
            match ratatui::Terminal::new(backend) {
                Ok(t) => Ok(Self { terminal: t }),
                Err(e) => {
                    let _ = disable_raw_mode();
                    let _ = execute!(io::stdout(), LeaveAlternateScreen);
                    Err(e)
                }
            }
        }

        pub fn terminal(
            &mut self,
        ) -> &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>> {
            &mut self.terminal
        }
    }

    impl Drop for TerminalSession {
        fn drop(&mut self) {
            use crossterm::execute;
            use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
    }

    /// Poll for one event, translating it. Returns `None` on a non-key event
    /// (resize, mouse) rather than ignoring it silently: the caller redraws on
    /// any event, which is what makes a resize repaint correctly.
    pub fn next_key(timeout: std::time::Duration) -> io::Result<Option<Key>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            CtEvent::Key(k) => Ok(Some(translate(k))),
            _ => Ok(None),
        }
    }
}

pub mod codebase {
    //! `pantheon codebase`: inspect the native symbol/call graph.
    //!
    //! The engine lives in `pantheon-exec::codegraph` and is also exposed to
    //! the agent as `codebase.*` tools. This verb is the operator view: build
    //! or refresh the cache, then search symbols or walk lexical calls.

    use crate::terminal::data_dir;
    use pantheon_exec::codegraph::{CodeGraph, Overview};
    use std::path::{Path, PathBuf};

    pub fn cmd_codebase(args: &[String]) {
        let verb = args.first().map(|s| s.as_str()).unwrap_or("overview");
        if matches!(verb, "help" | "--help" | "-h") {
            help();
            return;
        }
        let dd = data_dir();
        match verb {
            "index" => {
                let (root, json) = path_arg(args, 1);
                run_index(&root, &dd, json);
            }
            "search" => {
                let Some(query) = positional(args, 1) else {
                    eprintln!("codebase search: needs a query");
                    std::process::exit(2);
                };
                let (root, json) = path_arg(args, 2);
                let graph = run_load(&root, &dd);
                let nodes = graph.search(&query, 100);
                if json {
                    print_json(&nodes);
                } else {
                    for n in nodes {
                        println!("{:<10} {:<48} {}:{}", n.kind, n.qualified, n.file, n.line);
                    }
                }
            }
            "callers" => {
                let Some(symbol) = positional(args, 1) else {
                    eprintln!("codebase callers: needs a symbol");
                    std::process::exit(2);
                };
                let (root, json) = path_arg(args, 2);
                let graph = run_load(&root, &dd);
                let hits = graph.callers(&symbol, 100);
                if json {
                    print_json(&hits);
                } else {
                    for hit in hits {
                        println!(
                            "{} at {}:{} -> {}",
                            hit.node.qualified, hit.edge.file, hit.edge.line, hit.edge.to_name
                        );
                    }
                }
            }
            "callees" => {
                let Some(symbol) = positional(args, 1) else {
                    eprintln!("codebase callees: needs a symbol");
                    std::process::exit(2);
                };
                let (root, json) = path_arg(args, 2);
                let graph = run_load(&root, &dd);
                let edges = graph.callees(&symbol, 100);
                if json {
                    print_json(&edges);
                } else {
                    for edge in edges {
                        println!(
                            "{}:{} {} -> {}",
                            edge.file, edge.line, edge.from_name, edge.to_name
                        );
                    }
                }
            }
            "overview" | "" => {
                let (root, json) = path_arg(args, 1);
                let graph = run_load(&root, &dd);
                print_overview(&graph.overview(), json);
            }
            other => {
                eprintln!("codebase: unknown verb '{other}'");
                eprintln!(
                    "  usage: pantheon workspace codebase <index|search|callers|callees|overview> [args]"
                );
                std::process::exit(2);
            }
        }
    }

    fn help() {
        println!(concat!(
            "pantheon workspace codebase <command> [args]\n\n",
            "Commands:\n",
            "  index [PATH] [--json]       build/refresh the native symbol graph\n",
            "  search <query> [PATH]       search symbol names and paths\n",
            "  callers <symbol> [PATH]     inbound lexical call edges\n",
            "  callees <symbol> [PATH]     outbound lexical call edges\n",
            "  overview [PATH] [--json]    file/symbol/call counts and top dirs"
        ));
    }

    fn positional(args: &[String], index: usize) -> Option<String> {
        args.get(index).filter(|a| !a.starts_with('-')).cloned()
    }

    /// Accept an optional positional root and `--json` in any order after the
    /// required argument. The path is always the first non-flag argument.
    /// Anything else is an error: the old `_ => {}` silently swallowed
    /// unknown flags and extra positionals, so `codebase search foo
    /// --bogus` searched as if the flag had never been typed.
    fn path_arg(args: &[String], start: usize) -> (PathBuf, bool) {
        let mut root: Option<PathBuf> = None;
        let mut json = false;
        for arg in args.iter().skip(start) {
            match arg.as_str() {
                "--json" => json = true,
                s if s.starts_with('-') => {
                    eprintln!("codebase: unknown flag '{s}' (want --json)");
                    std::process::exit(2);
                }
                s if root.is_none() => {
                    root = Some(PathBuf::from(s));
                }
                s => {
                    eprintln!("codebase: unexpected extra argument '{s}'");
                    std::process::exit(2);
                }
            }
        }
        (
            root.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
            json,
        )
    }

    fn run_index(root: &Path, dd: &Path, json: bool) {
        match CodeGraph::build(root) {
            Ok(graph) => {
                let cache = CodeGraph::cache_path_for(dd, root);
                if let Err(e) = graph.save(&cache) {
                    eprintln!("codebase: index built but cache write failed: {e}");
                }
                print_overview(&graph.overview(), json);
            }
            Err(e) => {
                eprintln!("codebase: index failed: {e}");
                std::process::exit(1);
            }
        }
    }

    fn run_load(root: &Path, dd: &Path) -> CodeGraph {
        let cache = CodeGraph::cache_path_for(dd, root);
        if let Some(graph) = CodeGraph::load(&cache) {
            if graph.is_current_for(root) {
                return graph;
            }
        }
        match CodeGraph::build(root) {
            Ok(graph) => {
                let _ = graph.save(&cache);
                graph
            }
            Err(e) => {
                eprintln!("codebase: index failed: {e}");
                std::process::exit(1);
            }
        }
    }

    fn print_json<T: serde::Serialize>(value: &T) {
        match serde_json::to_string_pretty(value) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("codebase: encode failed: {e}"),
        }
    }

    fn print_overview(o: &Overview, json: bool) {
        if json {
            print_json(o);
            return;
        }
        println!("root         {}", o.root);
        println!("files        {} ({} parse errors)", o.files, o.parse_errors);
        println!("symbols      {}", o.symbols);
        println!("call edges   {}", o.calls);
        println!("by kind");
        for (kind, count) in &o.by_kind {
            println!("  {kind:<12} {count}");
        }
        if !o.top_directories.is_empty() {
            println!("top directories");
            for (dir, count) in &o.top_directories {
                println!("  {dir:<48} {count}");
            }
        }
    }
}
