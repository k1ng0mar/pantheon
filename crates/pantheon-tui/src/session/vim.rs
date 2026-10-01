//! Modal Vim editing for the composer (opt-in).
//!
//! v1 scope is deliberately small: Normal + Insert modes only. No Visual
//! mode, no macros, no registers beyond one internal yank register, no
//! `.vimrc`. The help text says so plainly.
//!
//! All buffer logic here is pure: it operates on `(&mut String, &mut
//! VimState)` and never touches `TuiState`, so it is unit-testable without
//! a terminal. The event loop in `session.rs` owns the `TuiState` glue
//! (key routing, overlay precedence, rendering).
//!
//! Cursor coordinates are char indices, never byte indices, so multi-byte
//! text cannot split a character. In Normal mode the cursor sits *on* a
//! character (clamped to the last char); in Insert mode it sits *between*
//! characters (may equal the line length).

/// Composer editing mode. Insert behaves like today's composer;
/// Normal interprets keys as motions and edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    /// Motions and edits; the default when vim is enabled.
    #[default]
    Normal,
    /// Type text; Esc returns to Normal.
    Insert,
}

/// Modal state for the composer. Lives on `TuiState` as `state.vim`.
#[derive(Debug, Clone)]
pub struct VimState {
    /// Master switch: `[tui] vim` in config.toml, toggled live by `/vim`.
    /// When false, every key path behaves exactly as it does today.
    pub enabled: bool,
    pub mode: VimMode,
    row: usize,
    col: usize,
    /// Undo snapshots of `(text, row, col)`, oldest-first, capped.
    undo: Vec<(String, usize, usize)>,
    /// Internal yank register (never touches the system clipboard).
    register: String,
    /// `true` when the register holds whole lines (`dd`).
    register_linewise: bool,
    /// Multi-key sequences: `Some('d')` after `d`, `Some('g')` after `g`.
    pending: Option<char>,
}

/// Cap on undo history: enough to recover from a bad edit spree,
// bounded so a long session cannot grow it without limit.
const UNDO_CAP: usize = 100;

impl VimState {
    pub fn new() -> Self {
        Self {
            enabled: false,
            mode: VimMode::Normal,
            row: 0,
            col: 0,
            undo: Vec::new(),
            register: String::new(),
            register_linewise: false,
            pending: None,
        }
    }

    /// Cursor as `(row, col)` char indices, clamped to `text`.
    pub fn cursor(&self, text: &str) -> (usize, usize) {
        let lines: Vec<&str> = text.split('\n').collect();
        let row = self.row.min(lines.len().saturating_sub(1));
        let len = lines.get(row).map(|l| l.chars().count()).unwrap_or(0);
        let col = match self.mode {
            // Normal sits on a character: never past the last one.
            VimMode::Normal => self.col.min(len.saturating_sub(1)),
            // Insert sits between characters: may equal the length.
            VimMode::Insert => self.col.min(len),
        };
        (row, col)
    }

    /// Status-bar label, e.g. `-- NORMAL --`; `None` when vim is off so
    /// non-users see no noise.
    pub fn status_label(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        Some(
            match self.mode {
                VimMode::Normal => "-- NORMAL --",
                VimMode::Insert => "-- INSERT --",
            }
            .to_string(),
        )
    }

    /// Reset after a submit: back to Normal, cursor home, history kept
    /// (undo across submits is a feature, not a bug).
    pub fn on_submit(&mut self) {
        self.mode = VimMode::Normal;
        self.row = 0;
        self.col = 0;
        self.pending = None;
    }

    /// Esc in Insert mode: drop back to Normal without touching the text
    /// or arming the interrupt/rewind. Clears a half-typed `d`/`g`.
    pub fn esc_to_normal(&mut self) {
        self.mode = VimMode::Normal;
        self.pending = None;
    }

    fn set_cursor(&mut self, row: usize, col: usize) {
        self.row = row;
        self.col = col;
    }

    fn push_undo(&mut self, text: &str) {
        if self.undo.last().is_some_and(|(t, _, _)| t == text) {
            return;
        }
        self.undo.push((text.to_string(), self.row, self.col));
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
    }
}

impl Default for VimState {
    fn default() -> Self {
        Self::new()
    }
}

/// Read `[tui] vim` from config.toml. Missing config or missing key =
/// disabled; vim is strictly opt-in.
pub fn load_vim(data_dir: &std::path::Path) -> bool {
    crate::config::Config::load(data_dir)
        .ok()
        .and_then(|c| c.tui)
        .and_then(|t| t.vim)
        .unwrap_or(false)
}

/// Persist the vim choice to `[tui] vim` in config.toml.
pub fn save_vim(
    data_dir: &std::path::Path,
    enabled: bool,
) -> Result<(), pantheon_api::error::PantheonError> {
    let mut cfg = crate::config::Config::load(data_dir).unwrap_or_default();
    let mut tui = cfg.tui.unwrap_or_default();
    tui.vim = Some(enabled);
    cfg.tui = Some(tui);
    cfg.save(data_dir)
}

/// Outcome of a key in Normal mode. The caller (`session.rs`) owns any
/// `TuiState`-level effects (quit, overlay); buffer edits are already
/// applied to `text`/`vim` when this returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalKey {
    /// Enter Insert mode; the cursor is already positioned.
    ToInsert,
    /// `q` on an empty buffer: quit the app, like `q` does today.
    Quit,
    /// `?` on an empty buffer: open the shortcuts overlay, like today.
    Shortcuts,
    /// Consumed as a motion/edit (or an unbound key: a no-op).
    Consumed,
    /// Not a vim binding; the caller decides (e.g. `v` = image preview).
    Pass(char),
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn split_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_string).collect()
}

fn line_len(lines: &[String], row: usize) -> usize {
    lines.get(row).map(|l| l.chars().count()).unwrap_or(0)
}

/// Clamp `(row, col)` to `lines` for Normal mode (cursor on a char).
fn clamp_normal(lines: &[String], row: usize, col: usize) -> (usize, usize) {
    let row = row.min(lines.len().saturating_sub(1));
    let len = line_len(lines, row);
    (row, col.min(len.saturating_sub(1)))
}

/// Char at `(row, col)`, or `None` past the end.
fn char_at(lines: &[String], row: usize, col: usize) -> Option<char> {
    lines.get(row)?.chars().nth(col)
}

/// Flattened position of `(row, col)`: total chars before it, counting
/// one virtual char per newline.
fn flat_pos(lines: &[String], row: usize, col: usize) -> usize {
    let mut pos = 0;
    for (i, line) in lines.iter().enumerate() {
        if i == row {
            return pos + col.min(line.chars().count());
        }
        pos += line.chars().count() + 1; // +1 for the newline
    }
    pos
}

/// `(row, col)` of a flattened position.
fn unflat_pos(lines: &[String], mut pos: usize) -> (usize, usize) {
    for (i, line) in lines.iter().enumerate() {
        let len = line.chars().count();
        if pos <= len || i + 1 == lines.len() {
            return (i, pos.min(len));
        }
        pos -= len + 1;
    }
    (lines.len().saturating_sub(1), 0)
}

fn total_chars(lines: &[String]) -> usize {
    flat_pos(lines, lines.len().saturating_sub(1), usize::MAX)
}

/// Start of the next word at/after `pos` (flattened). `None` = no more
/// words; the caller falls back to end-of-buffer.
fn next_word_start(lines: &[String], pos: usize) -> Option<usize> {
    let total = total_chars(lines);
    let mut p = pos;
    // If on a word char, skip the rest of this word first.
    let (mut r, mut c) = unflat_pos(lines, p);
    if char_at(lines, r, c).is_some_and(is_word_char) {
        while p < total {
            let (rr, cc) = unflat_pos(lines, p);
            if !char_at(lines, rr, cc).is_some_and(is_word_char) {
                break;
            }
            p += 1;
        }
    }
    // Skip non-word chars to the next word start.
    while p < total {
        let (rr, cc) = unflat_pos(lines, p);
        r = rr;
        c = cc;
        if char_at(lines, r, c).is_some_and(is_word_char) {
            return Some(p);
        }
        p += 1;
    }
    None
}

/// Start of the word at/before `pos`: if `pos` is at a word start, the
/// previous word's start; otherwise the current word's start.
fn prev_word_start(lines: &[String], pos: usize) -> Option<usize> {
    if pos == 0 {
        return None;
    }
    let mut p = pos - 1;
    // Skip non-word chars backward.
    loop {
        let (r, c) = unflat_pos(lines, p);
        if char_at(lines, r, c).is_some_and(is_word_char) {
            break;
        }
        if p == 0 {
            return None;
        }
        p -= 1;
    }
    // Skip word chars backward to the word start.
    loop {
        let (r, c) = unflat_pos(lines, p);
        if !char_at(lines, r, c).is_some_and(is_word_char) {
            return Some(p + 1);
        }
        if p == 0 {
            return Some(0);
        }
        p -= 1;
    }
}

/// End of the word at/after `pos` (last word char).
fn word_end(lines: &[String], pos: usize) -> Option<usize> {
    let total = total_chars(lines);
    let mut p = pos;
    // Skip non-word chars to the next word.
    while p < total {
        let (r, c) = unflat_pos(lines, p);
        if char_at(lines, r, c).is_some_and(is_word_char) {
            break;
        }
        p += 1;
    }
    if p >= total {
        return None;
    }
    // Advance to the last word char.
    let mut end = p;
    while end + 1 < total {
        let (r, c) = unflat_pos(lines, end + 1);
        if !char_at(lines, r, c).is_some_and(is_word_char) {
            break;
        }
        end += 1;
    }
    // If already at the end, move to the next word's end instead (vim `e`).
    if end == pos {
        if let Some(ns) = next_word_start(lines, pos + 1) {
            return word_end(lines, ns);
        }
    }
    Some(end)
}

fn first_non_blank(lines: &[String], row: usize) -> usize {
    lines
        .get(row)
        .map(|l| l.chars().position(|c| c != ' ' && c != '\t').unwrap_or(0))
        .unwrap_or(0)
}

fn remove_range(lines: &mut Vec<String>, from: usize, to: usize) -> String {
    // Remove flattened `[from, to)`, return the removed text.
    let text = lines.join("\n");
    let chars: Vec<char> = text.chars().collect();
    let (from, to) = (from.min(chars.len()), to.min(chars.len()));
    let removed: String = chars[from..to].iter().collect();
    let rest: String = chars[..from].iter().chain(chars[to..].iter()).collect();
    *lines = split_lines(&rest);
    if lines.is_empty() {
        lines.push(String::new());
    }
    removed
}

/// Interpret one key in Normal mode, mutating `text`/`vim` for edits and
/// motions. Returns what the caller must do at the `TuiState` level.
pub fn handle_normal_key(text: &mut String, vim: &mut VimState, key: char) -> NormalKey {
    let mut lines = split_lines(text);
    let (row, col) = clamp_normal(&lines, vim.row, vim.col);
    vim.set_cursor(row, col);

    // Multi-key sequences first.
    if let Some(p) = vim.pending {
        vim.pending = None;
        match (p, key) {
            ('d', 'd') => {
                vim.push_undo(text);
                let yanked = lines.remove(row);
                if lines.is_empty() {
                    lines.push(String::new());
                }
                vim.register = yanked;
                vim.register_linewise = true;
                let row = row.min(lines.len() - 1);
                vim.set_cursor(row, 0);
                *text = lines.join("\n");
                return NormalKey::Consumed;
            }
            ('d', 'w') => {
                vim.push_undo(text);
                let pos = flat_pos(&lines, row, col);
                let end = next_word_start(&lines, pos).unwrap_or_else(|| {
                    // No next word: delete to end of line (vim `dw`).
                    flat_pos(&lines, row, line_len(&lines, row))
                });
                vim.register = remove_range(&mut lines, pos, end);
                vim.register_linewise = false;
                let (r, c) = clamp_normal(&lines, row, col);
                vim.set_cursor(r, c);
                *text = lines.join("\n");
                return NormalKey::Consumed;
            }
            ('d', 'b') => {
                vim.push_undo(text);
                let pos = flat_pos(&lines, row, col);
                if let Some(start) = prev_word_start(&lines, pos) {
                    vim.register = remove_range(&mut lines, start, pos);
                    vim.register_linewise = false;
                    let (r, c) = unflat_pos(&lines, start);
                    let (r, c) = clamp_normal(&lines, r, c);
                    vim.set_cursor(r, c);
                    *text = lines.join("\n");
                }
                return NormalKey::Consumed;
            }
            ('g', 'g') => {
                let c = first_non_blank(&lines, 0);
                vim.set_cursor(0, c);
                return NormalKey::Consumed;
            }
            _ => return NormalKey::Consumed, // unknown sequence: drop it
        }
    }

    match key {
        // --- insert entries: position the cursor, caller flips the mode ---
        'i' => NormalKey::ToInsert,
        'a' => {
            // Append after the cursor: one char right, clamped to the
            // insert-style end of line.
            let len = line_len(&lines, row);
            vim.set_cursor(row, (col + 1).min(len));
            NormalKey::ToInsert
        }
        'I' => {
            vim.set_cursor(row, first_non_blank(&lines, row));
            NormalKey::ToInsert
        }
        'A' => {
            vim.set_cursor(row, line_len(&lines, row));
            NormalKey::ToInsert
        }
        'o' => {
            vim.push_undo(text);
            lines.insert(row + 1, String::new());
            vim.set_cursor(row + 1, 0);
            *text = lines.join("\n");
            NormalKey::ToInsert
        }
        'O' => {
            vim.push_undo(text);
            lines.insert(row, String::new());
            vim.set_cursor(row, 0);
            *text = lines.join("\n");
            NormalKey::ToInsert
        }
        // --- motions ---
        'h' => {
            if col > 0 {
                vim.set_cursor(row, col - 1);
            }
            NormalKey::Consumed
        }
        'l' => {
            let len = line_len(&lines, row);
            if col + 1 < len {
                vim.set_cursor(row, col + 1);
            }
            NormalKey::Consumed
        }
        'j' => {
            if row + 1 < lines.len() {
                let (_, c) = clamp_normal(&lines, row + 1, col);
                vim.set_cursor(row + 1, c);
            }
            NormalKey::Consumed
        }
        'k' => {
            if row > 0 {
                let (_, c) = clamp_normal(&lines, row - 1, col);
                vim.set_cursor(row - 1, c);
            }
            NormalKey::Consumed
        }
        'w' => {
            let pos = flat_pos(&lines, row, col);
            match next_word_start(&lines, pos) {
                Some(p) => {
                    let (r, c) = unflat_pos(&lines, p);
                    let (r, c) = clamp_normal(&lines, r, c);
                    vim.set_cursor(r, c);
                }
                None => {
                    // No more words: end of buffer.
                    let last = lines.len() - 1;
                    let (_, c) = clamp_normal(&lines, last, usize::MAX);
                    vim.set_cursor(last, c);
                }
            }
            NormalKey::Consumed
        }
        'b' => {
            let pos = flat_pos(&lines, row, col);
            if let Some(p) = prev_word_start(&lines, pos) {
                let (r, c) = unflat_pos(&lines, p);
                let (r, c) = clamp_normal(&lines, r, c);
                vim.set_cursor(r, c);
            }
            NormalKey::Consumed
        }
        'e' => {
            let pos = flat_pos(&lines, row, col);
            if let Some(p) = word_end(&lines, pos) {
                let (r, c) = unflat_pos(&lines, p);
                let (r, c) = clamp_normal(&lines, r, c);
                vim.set_cursor(r, c);
            }
            NormalKey::Consumed
        }
        '0' => {
            vim.set_cursor(row, 0);
            NormalKey::Consumed
        }
        '$' => {
            let (_, c) = clamp_normal(&lines, row, usize::MAX);
            vim.set_cursor(row, c);
            NormalKey::Consumed
        }
        'G' => {
            let last = lines.len() - 1;
            vim.set_cursor(last, first_non_blank(&lines, last));
            NormalKey::Consumed
        }
        // --- edits ---
        'x' => {
            if line_len(&lines, row) > 0 {
                vim.push_undo(text);
                let mut chars: Vec<char> = lines[row].chars().collect();
                let yanked = chars.remove(col.min(chars.len() - 1));
                lines[row] = chars.into_iter().collect();
                vim.register = yanked.to_string();
                vim.register_linewise = false;
                let (r, c) = clamp_normal(&lines, row, col);
                vim.set_cursor(r, c);
                *text = lines.join("\n");
            }
            NormalKey::Consumed
        }
        'u' => {
            if let Some((t, r, c)) = vim.undo.pop() {
                *text = t;
                let lines = split_lines(text);
                let (r, c) = clamp_normal(&lines, r, c);
                vim.set_cursor(r, c);
            }
            NormalKey::Consumed
        }
        'p' => {
            if !vim.register.is_empty() {
                vim.push_undo(text);
                if vim.register_linewise {
                    // Pasting onto a buffer that `dd` emptied restores the
                    // line in place instead of leaving a stray blank line.
                    if lines.len() == 1 && lines[0].is_empty() {
                        lines[0] = vim.register.clone();
                        vim.set_cursor(0, 0);
                    } else {
                        lines.insert(row + 1, vim.register.clone());
                        vim.set_cursor(row + 1, 0);
                    }
                } else {
                    let mut chars: Vec<char> = lines[row].chars().collect();
                    let at = (col + 1).min(chars.len());
                    let reg: Vec<char> = vim.register.chars().collect();
                    let n = reg.len();
                    for (i, ch) in reg.into_iter().enumerate() {
                        chars.insert(at + i, ch);
                    }
                    lines[row] = chars.into_iter().collect();
                    let (r, c) = clamp_normal(&lines, row, at + n - 1);
                    vim.set_cursor(r, c);
                }
                *text = lines.join("\n");
            }
            NormalKey::Consumed
        }
        'd' => {
            vim.pending = Some('d');
            NormalKey::Consumed
        }
        'g' => {
            vim.pending = Some('g');
            NormalKey::Consumed
        }
        // --- app affordances preserved on an empty buffer ---
        '?' if text.is_empty() => NormalKey::Shortcuts,
        'q' if text.is_empty() => NormalKey::Quit,
        _ => NormalKey::Pass(key),
    }
}

/// Insert one char at the cursor (Insert mode). The cursor advances.
pub fn insert_char(text: &mut String, vim: &mut VimState, c: char) {
    let mut lines = split_lines(text);
    let row = vim.row.min(lines.len().saturating_sub(1));
    let len = line_len(&lines, row);
    let col = vim.col.min(len);
    let mut chars: Vec<char> = lines[row].chars().collect();
    chars.insert(col, c);
    lines[row] = chars.into_iter().collect();
    vim.set_cursor(row, col + 1);
    *text = lines.join("\n");
}

/// Backspace in Insert mode: delete the char before the cursor; at
/// column 0, join with the previous line.
pub fn backspace(text: &mut String, vim: &mut VimState) {
    let mut lines = split_lines(text);
    let row = vim.row.min(lines.len().saturating_sub(1));
    let len = line_len(&lines, row);
    let col = vim.col.min(len);
    if col > 0 {
        let mut chars: Vec<char> = lines[row].chars().collect();
        chars.remove(col - 1);
        lines[row] = chars.into_iter().collect();
        vim.set_cursor(row, col - 1);
        *text = lines.join("\n");
    } else if row > 0 {
        let tail = lines.remove(row);
        let new_col = line_len(&lines, row - 1);
        lines[row - 1].push_str(&tail);
        vim.set_cursor(row - 1, new_col);
        *text = lines.join("\n");
    }
}

/// Move the cursor to the end of the input (used before `@` mentions,
/// which complete at end-of-input).
pub fn move_to_end(text: &str, vim: &mut VimState) {
    let lines = split_lines(text);
    let row = lines.len().saturating_sub(1);
    vim.set_cursor(row, line_len(&lines, row));
    vim.mode = VimMode::Insert;
}

/// Snapshot the buffer before an insert session so `u` can undo it whole.
pub fn begin_insert_undo(text: &str, vim: &mut VimState) {
    vim.push_undo(text);
}

// ---------------------------------------------------------------------------
// Small deterministic invariant unit tests. Behavioral, integration, and
// timing tests live in `pantheon-eval`, never here.
// ---------------------------------------------------------------------------
