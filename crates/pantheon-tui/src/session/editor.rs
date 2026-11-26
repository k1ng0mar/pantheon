//! fullscreen draft editor for long prompts.
//!
//! tiny multi-line buffer with a visible cursor: no `$EDITOR`, no subprocess,
//! no temp files. opens with the current draft, `Ctrl+Enter` (or `Ctrl+S`)
//! sends, `Esc` cancels and keeps the draft in the input box. all ops are
//! char-boundary safe

/// the editable draft. `col` is a char index into the line, never a byte
/// index, so multi-byte text can't split a character
#[derive(Debug, Clone)]
pub struct DraftEditor {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl DraftEditor {
    /// open on existing text; cursor starts at the end
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

    /// the draft as one string, newlines joined
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
        self.lines.iter().map(|l| l.chars().count()).sum::<usize>()
            + self.lines.len().saturating_sub(1)
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

    pub fn newline(&mut self) {
        let chars = self.line_chars();
        let col = self.col.min(chars.len());
        let (head, tail): (Vec<char>, Vec<char>) = (chars[..col].to_vec(), chars[col..].to_vec());
        self.lines[self.row] = head.iter().collect();
        self.lines.insert(self.row + 1, tail.iter().collect());
        self.row += 1;
        self.col = 0;
    }

    /// delete the char before the cursor; at col 0, join with the previous line
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

    /// delete the char under the cursor; at end of line, pull the next one up
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
        // at the first line there's nowhere to go: stay put instead of
        // resetting col to 0 (that lost the column and made Up on the top
        // line a destructive move)
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn move_down(&mut self) {
        // same at the last line: stay put instead of jumping to line end
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }
}

#[cfg(test)]
mod move_edge_tests {
    use super::*;

    #[test]
    fn move_up_at_first_line_keeps_column() {
        let mut ed = DraftEditor::from_text("hello\nworld");
        // cursor starts at end of last line; move to first line col 3
        ed.move_up();
        ed.move_left();
        ed.move_left();
        assert_eq!(ed.cursor(), (0, 3));
        ed.move_up();
        assert_eq!(ed.cursor(), (0, 3));
    }

    #[test]
    fn move_down_at_last_line_keeps_column() {
        let mut ed = DraftEditor::from_text("hello\nworld");
        ed.move_up();
        ed.move_left();
        ed.move_left();
        assert_eq!(ed.cursor(), (0, 3));
        ed.move_down();
        assert_eq!(ed.cursor(), (1, 3));
        ed.move_down();
        assert_eq!(ed.cursor(), (1, 3));
    }
}
