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
// SearchList
// ---------------------------------------------------------------------------

/// Browsing a collection the runtime owns rather than a fixed menu: sessions,
/// runs, skills, tools, providers-with-live-state.
///
/// Separate from [`Select`] because the list can be empty for a reason the
/// user needs stated ("no runs yet") and because rows carry a status that
/// reflects something outside the widget.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchList {
    pub title: String,
    pub hint: String,
    /// Rendered when there are no rows at all, which is different from
    /// "your filter matched nothing".
    pub empty_reason: String,
    pub list: ListState,
    /// Set when the collection is being loaded, so the widget can say so
    /// instead of claiming the list is empty.
    pub loading: bool,
}

impl SearchList {
    pub fn new(
        title: impl Into<String>,
        items: Vec<Item>,
        empty_reason: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            hint: "type to filter · up/down · enter open · esc back".into(),
            empty_reason: empty_reason.into(),
            loading: false,
            list: ListState::new(items),
        }
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
// list rendering, shared by every list widget
// ---------------------------------------------------------------------------

/// A row ready to draw. The renderer turns this into a ratatui `Line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub marker: String,
    pub label: String,
    pub desc: String,
    pub tag: String,
    pub selected: bool,
    pub enabled: bool,
}

/// Build the drawable rows for any list-backed widget.
///
/// One function for all four list widgets, so a selected row cannot be styled
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

/// Search-list rows.
pub fn search_rows(s: &SearchList) -> Vec<Row> {
    rows_for(&s.list, None)
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