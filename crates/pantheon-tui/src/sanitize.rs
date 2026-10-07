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
