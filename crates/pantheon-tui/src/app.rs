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
    SearchList(crate::widget::SearchList),
    /// A screen with no widget of its own: the session view, the finished
    /// screen, anything that handles its own keys.
    Raw(RawScreen),
}

/// A screen that renders and handles keys itself.
pub trait RawHandler: Send {
    /// A ratatui frame, to draw into.
    fn draw(&mut self, f: &mut ratatui::Frame);
    /// Handle a key. Return `None` to fall through to the next screen.
    fn key(&mut self, key: Key) -> Option<ScreenResult>;
    /// How tall this screen wants the shared status area to be, in rows.
    fn status_rows(&self) -> u16 {
        1
    }
}

pub struct RawScreen {
    pub handler: Box<dyn RawHandler>,
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
    pub fn search(title: impl Into<String>, w: crate::widget::SearchList) -> Self {
        Self {
            title: title.into(),
            widget: ScreenWidget::SearchList(w),
            on_finish: None,
        }
    }
    pub fn raw(title: impl Into<String>, h: Box<dyn RawHandler>) -> Self {
        Self {
            title: title.into(),
            widget: ScreenWidget::Raw(RawScreen { handler: h }),
            on_finish: None,
        }
    }

    /// Set the continuation run when this screen finishes.
    pub fn then<F: FnOnce(Selection) + Send + 'static>(mut self, f: F) -> Self {
        self.on_finish = Some(Box::new(f));
        self
    }

    pub fn is_raw(&self) -> bool {
        matches!(self.widget, ScreenWidget::Raw(_))
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

    /// The base screen is whatever is at the bottom of the stack; the session
    /// view lives there and every overlay stacks above it.
    pub fn base(&self) -> Option<&Screen> {
        self.stack.first()
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
                ScreenWidget::SearchList(w) => submit(w.handle_key(key)),
                ScreenWidget::Raw(r) => r.handler.key(key),
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
                ScreenWidget::SearchList(w) => crate::render::draw_search(f, target, w),
                ScreenWidget::Raw(r) => r.handler.draw(f),
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
            disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
        };
        enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
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

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
