//! turn timeline: a navigable rail of conversation turns.
//!
//! one entry per user turn, in conversation order. read-only: moving the
//! selection and jumping the transcript viewport never mutates the transcript.
//! (dsh-TUI's TimelineRail was the inspiration; keyboard-driven reimplementation,
//! not a port.)

use super::{BlockKind, TranscriptBlock};

/// one turn in the rail: the user message that opened it + where its blocks
/// start in the transcript
#[derive(Debug, Clone)]
pub struct TimelineTurn {
    /// 1-based turn number, in conversation order
    pub turn_no: usize,
    /// index of the turn's first block (the user message) in `TuiState::blocks`
    pub block_start: usize,
    /// first line of the user message, truncated for the rail
    pub preview: String,
}

/// build the rail from the transcript. a turn opens at every user message;
/// status/assistant/tool blocks belong to the turn above them. blocks before
/// the first user message (welcome status lines) aren't turns
pub fn build_turns(blocks: &[TranscriptBlock]) -> Vec<TimelineTurn> {
    let mut turns = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        if let BlockKind::UserMessage(text) = &block.kind {
            let preview: String = text.lines().next().unwrap_or("").chars().take(48).collect();
            turns.push(TimelineTurn {
                turn_no: turns.len() + 1,
                block_start: i,
                preview,
            });
        }
    }
    turns
}

/// first visible row of the rail for a given selection: the window follows the
/// selection, not the tail, so moving the selection above the tail window keeps
/// it visible. pure, so tests cover it directly
pub fn window_start(sel: usize, total: usize, rows: usize) -> usize {
    let rows = rows.max(1);
    if total <= rows || sel < rows {
        0
    } else {
        (sel + 1 - rows).min(total - rows)
    }
}

/// selection state for the open rail. index into the `build_turns` list; it
/// clamps, never wraps, so arrowing past either end is a harmless no-op
#[derive(Debug, Clone)]
pub struct TimelineNav {
    sel: usize,
}

impl TimelineNav {
    /// open with the selection on the latest turn (the live tail)
    pub fn open(turn_count: usize) -> Self {
        Self {
            sel: turn_count.saturating_sub(1),
        }
    }

    pub fn sel(&self) -> usize {
        self.sel
    }

    /// move the selection by `n`, clamped to the turn list
    pub fn move_sel(&mut self, n: isize, turn_count: usize) {
        if turn_count == 0 {
            self.sel = 0;
            return;
        }
        let sel = self.sel as isize + n;
        self.sel = sel.clamp(0, turn_count as isize - 1) as usize;
    }
}
