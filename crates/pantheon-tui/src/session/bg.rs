//! Background tasks (`/btw`): fire-and-forget turns that run beside the
//! main session without interrupting it.
//!
//! Lifecycle: queued → running → done | failed. The TUI owns the task
//! list; each task executes its turn on a worker thread against its own
//! run id with its own cancel token, so double-Esc on the main turn never
//! touches a background task and vice versa. Completion arrives as
//! `TuiEvent::BgDone` and lands in the transcript as a labeled
//! `BlockKind::BgResult` block.

use std::sync::{atomic::AtomicBool, Arc};
use std::time::{SystemTime, UNIX_EPOCH};

/// Max background tasks alive (queued + running) at once.
pub const MAX_BG_TASKS: usize = 4;

/// Max chars of the prompt shown in labels and lists.
pub const LABEL_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgStatus {
    Queued,
    Running,
    Done,
    Failed,
}

impl BgStatus {
    pub fn is_active(self) -> bool {
        matches!(self, BgStatus::Queued | BgStatus::Running)
    }

    pub fn word(self) -> &'static str {
        match self {
            BgStatus::Queued => "queued",
            BgStatus::Running => "running",
            BgStatus::Done => "done",
            BgStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BgTask {
    pub id: u64,
    pub label: String,
    pub status: BgStatus,
    pub run_id: String,
    pub parent_run_id: String,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub output: Option<String>,
    /// Independent cancel token: the main turn's double-Esc writes the
    /// session token, never this one.
    pub cancel: Arc<AtomicBool>,
    /// Tool calls captured from this task's own run id, for the sidebar
    /// activity timeline. Bounded; see [`MAX_BG_STEPS`].
    pub steps: Vec<super::activity::BgStep>,
}

/// Max captured tool steps per background task (timeline display cap).
pub const MAX_BG_STEPS: usize = 12;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Short, secret-free label for lists and result blocks: first line of
/// the prompt, run through the log redactor, capped at `LABEL_CHARS`.
pub fn task_label(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("").trim();
    let redacted = pantheon_api::logging::redact(first);
    let mut label: String = redacted.chars().take(LABEL_CHARS - 3).collect();
    if redacted.chars().count() > LABEL_CHARS {
        label.push_str("...");
    }
    if label.is_empty() {
        label.push_str("(empty prompt)");
    }
    label
}

impl BgTask {
    pub fn new(id: u64, prompt: &str, run_id: String, parent_run_id: String) -> Self {
        BgTask {
            id,
            label: task_label(prompt),
            status: BgStatus::Queued,
            run_id,
            parent_run_id,
            started_ms: now_ms(),
            finished_ms: None,
            output: None,
            cancel: Arc::new(AtomicBool::new(false)),
            steps: Vec::new(),
        }
    }

    pub fn mark_running(&mut self) {
        if self.status == BgStatus::Queued {
            self.status = BgStatus::Running;
        }
    }

    pub fn finish(&mut self, output: String) {
        self.status = BgStatus::Done;
        self.finished_ms = Some(now_ms());
        self.output = Some(output);
    }

    pub fn fail(&mut self, err: String) {
        self.status = BgStatus::Failed;
        self.finished_ms = Some(now_ms());
        self.output = Some(err);
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.finished_ms
            .unwrap_or_else(now_ms)
            .saturating_sub(self.started_ms)
    }
}

/// How many tasks are still alive (queued or running).
pub fn active_count(tasks: &[BgTask]) -> usize {
    tasks.iter().filter(|t| t.status.is_active()).count()
}

/// True when another background task may start.
pub fn can_spawn(tasks: &[BgTask]) -> bool {
    active_count(tasks) < MAX_BG_TASKS
}

/// First line of a result, capped - for notification summaries.
pub fn summary_line(output: &str) -> String {
    output
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// Spinner frame for the status bar, driven by wall-clock millis.
/// Pure in the clock value, so tests can pin it.
pub fn spinner_frame(now_ms: u64) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(now_ms as usize / 200) % FRAMES.len()]
}

/// Status-bar segment, e.g. `"bg 2 ⠋"`; `None` when nothing is alive so
/// the bar stays quiet for operators who never use `/btw`.
pub fn status_segment(tasks: &[BgTask]) -> Option<String> {
    let n = active_count(tasks);
    if n == 0 {
        None
    } else {
        Some(format!("bg {n} {}", spinner_frame(now_ms())))
    }
}

/// Header line for the transcript result block.
pub fn result_header(task: &BgTask) -> String {
    match task.status {
        BgStatus::Done => format!("◈ background result bg-{} · \"{}\"", task.id, task.label),
        BgStatus::Failed => format!(
            "× background task bg-{} failed · \"{}\"",
            task.id, task.label
        ),
        _ => format!("● background task bg-{} · \"{}\"", task.id, task.label),
    }
}
