//! Turn timeline: a navigable rail of conversation turns.
//!
//! One entry per user turn, in conversation order. Navigation is strictly
//! read-only: moving the selection and jumping the transcript viewport
//! never mutates the transcript. (dsh-TUI's TimelineRail was the
//! inspiration; this is a keyboard-driven reimplementation, not a port.)

use super::{BlockKind, TranscriptBlock};

/// One turn in the rail: the user message that opened it plus where its
/// blocks start in the transcript.
#[derive(Debug, Clone)]
pub struct TimelineTurn {
    /// 1-based turn number in conversation order.
    pub turn_no: usize,
    /// Index of the turn's first block (the user message) in
    /// `TuiState::blocks`.
    pub block_start: usize,
    /// First line of the user message, truncated for the rail.
    pub preview: String,
}

/// Build the rail from the transcript. A turn opens at every user message;
/// status/assistant/tool blocks belong to the turn above them. Blocks
/// before the first user message (welcome status lines) are not turns.
pub fn build_turns(blocks: &[TranscriptBlock]) -> Vec<TimelineTurn> {
    let mut turns = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        if let BlockKind::UserMessage(text) = &block.kind {
            let preview: String = text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(48)
                .collect();
            turns.push(TimelineTurn {
                turn_no: turns.len() + 1,
                block_start: i,
                preview,
            });
        }
    }
    turns
}

/// Selection state for the open rail. The selection is an index into the
/// `build_turns` list; it clamps, never wraps, so arrowing past either end
/// is a harmless no-op.
#[derive(Debug, Clone)]
pub struct TimelineNav {
    sel: usize,
}

impl TimelineNav {
    /// Open with the selection on the latest turn (the live tail).
    pub fn open(turn_count: usize) -> Self {
        Self {
            sel: turn_count.saturating_sub(1),
        }
    }

    pub fn sel(&self) -> usize {
        self.sel
    }

    /// Move the selection by `n`, clamped to the turn list.
    pub fn move_sel(&mut self, n: isize, turn_count: usize) {
        if turn_count == 0 {
            self.sel = 0;
            return;
        }
        let sel = self.sel as isize + n;
        self.sel = sel.clamp(0, turn_count as isize - 1) as usize;
    }
}
