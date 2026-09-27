//! Fullscreen draft editor for long prompts.
//!
//! A tiny multi-line buffer with a visible cursor: no `$EDITOR`, no
//! subprocess, no temp files. Opens with the current draft, `Ctrl+Enter`
//! (or `Ctrl+S`) sends, `Esc` cancels and keeps the draft in the input
//! box. All operations are char-boundary safe.

/// The editable draft. `col` is a char index into the line, never a byte
/// index, so multi-byte text cannot split a character.
#[derive(Debug, Clone)]
pub struct DraftEditor {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl DraftEditor {
    /// Open on existing text; the cursor starts at the end of the draft.
    pub fn from_text(text: &str) -> Self {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        let row = lines.len() - 1;
        let col = lines[row].chars().count();
        Self { lines, row, col }
    }

    /// The draft as one string, newlines joined.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn char_count(&self) -> usize {
        self.lines.iter().map(|l| l.chars().count()).sum::<usize>() + self.lines.len().saturating_sub(1)
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    fn line_chars(&self) -> Vec<char> {
        self.lines[self.row].chars().collect()
    }

    fn set_line(&mut self, chars: &[char]) {
        self.lines[self.row] = chars.iter().collect();
    }

    pub fn insert_char(&mut self, c: char) {
        let mut chars = self.line_chars();
        let col = self.col.min(chars.len());
        chars.insert(col, c);
        self.set_line(&chars);
        self.col = col + 1;
    }

    /// Split the line at the cursor.
    pub fn newline(&mut self) {
        let chars = self.line_chars();
        let col = self.col.min(chars.len());
        let (head, tail): (Vec<char>, Vec<char>) =
            (chars[..col].to_vec(), chars[col..].to_vec());
        self.lines[self.row] = head.iter().collect();
        self.lines.insert(self.row + 1, tail.iter().collect());
        self.row += 1;
        self.col = 0;
    }

    /// Delete the char before the cursor; at column 0, join with the
    /// previous line.
    pub fn backspace(&mut self) {
        if self.col > 0 {
            let mut chars = self.line_chars();
            let col = self.col.min(chars.len());
            chars.remove(col - 1);
            self.set_line(&chars);
            self.col = col - 1;
        } else if self.row > 0 {
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&tail);
        }
    }

    /// Delete the char under the cursor; at end of line, join the next
    /// line up.
    pub fn delete(&mut self) {
        let len = self.line_chars().len();
        if self.col < len {
            let mut chars = self.line_chars();
            chars.remove(self.col);
            self.set_line(&chars);
        } else if self.row + 1 < self.lines.len() {
            let tail = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&tail);
        }
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        } else {
            self.col = 0;
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        } else {
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }
}
