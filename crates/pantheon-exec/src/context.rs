//! Context-window budget (sibling of output compaction): deterministic
//! fitting of the assembled transcript to the model's window BEFORE the
//! provider call, so long runs degrade by dropping the oldest context
//! instead of dying on a provider 400.
//!
//! Rules (all deterministic, no model in the loop):
//! 1. Under budget: nothing changes.
//! 2. Re-compact the oldest oversized tool rows to a hard floor (data goes,
//!    instructions stay — same head+tail rule as `compact_output`).
//! 3. Drop oldest *whole exchanges* (a user row plus every row up to the
//!    next user row). System rows, the preamble before the first user row,
//!    and the final exchange are never dropped — dropping whole exchanges
//!    keeps assistant/tool_call pairing valid on the wire.
//! 4. Still over with only essential rows left: structured
//!    `CONTEXT_OVERFLOW` (host fixes the prompt or picks a bigger window).
//!
//! The estimate is bytes/4 plus per-row overhead — an estimate, padded by a
//! safety factor. It is not a tokenizer; it only has to trigger trimming
//! before the provider rejects the request.

use crate::{compact_output, CompactionPolicy};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::{Message, Role};
use pantheon_api::model::{CompressionRequest, ContextCompressor};

/// Tool rows are re-compacted down to this floor (bytes) in step 2.
pub const TOOL_FLOOR_BYTES: usize = 2 * 1024;
/// Estimator margin: fit to 85% of the usable window, since bytes/4 is
/// approximate and providers count wire overhead we cannot see.
pub const DEFAULT_SAFETY: f32 = 0.85;

/// Rough token estimate for a text blob: bytes/4, rounded up.
pub fn estimate_tokens(text: &str) -> u32 {
    (text.len() as u32).div_ceil(4)
}

/// Estimated tokens for one row: content + fixed row overhead + tool-call
/// payloads (they ride as JSON on the wire, so they must be counted).
pub fn row_tokens(m: &Message) -> u32 {
    let mut t = estimate_tokens(&m.content) + 8;
    for c in &m.tool_calls {
        t += estimate_tokens(&c.name) + estimate_tokens(&c.arguments) + 16;
    }
    t
}

/// Estimated input tokens for a whole transcript.
pub fn estimate_messages(messages: &[Message]) -> u32 {
    messages
        .iter()
        .map(row_tokens)
        .fold(0u32, |a, b| a.saturating_add(b))
}

/// What the fit may spend on input tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowBudget {
    /// Full context window (catalog `context_limit`).
    pub limit: u32,
    /// Tokens reserved for the completion (catalog `max_output_tokens`).
    pub reserve_output: u32,
    /// Multiplier applied to the remainder (estimator margin).
    pub safety: f32,
}

impl WindowBudget {
    pub fn new(limit: u32, reserve_output: u32) -> Self {
        Self {
            limit,
            reserve_output,
            safety: DEFAULT_SAFETY,
        }
    }

    /// Usable input tokens after reserving output room and padding.
    pub fn usable(&self) -> u32 {
        let avail = self.limit.saturating_sub(self.reserve_output) as f32;
        (avail * self.safety) as u32
    }
}

/// What the fit did, for the ledger event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FitReport {
    /// Estimated input tokens after fitting.
    pub estimated: u32,
    /// The usable window it fitted to.
    pub window: u32,
    /// Rows dropped (oldest whole exchanges).
    pub dropped_rows: u32,
    /// Tool rows re-compacted to the floor.
    pub compacted_rows: u32,
}

impl FitReport {
    /// True when the transcript the model sees differs from the original.
    pub fn changed(&self) -> bool {
        self.dropped_rows > 0 || self.compacted_rows > 0
    }
}

/// Fit `messages` into `budget`. Returns the (possibly trimmed) transcript
/// plus a report; `CONTEXT_OVERFLOW` when even the essential rows do not fit.
pub fn fit_to_window(
    mut messages: Vec<Message>,
    budget: &WindowBudget,
) -> Result<(Vec<Message>, FitReport), PantheonError> {
    let window = budget.usable();
    let mut estimated = estimate_messages(&messages);
    let mut report = FitReport {
        estimated,
        window,
        ..Default::default()
    };
    if estimated <= window {
        return Ok((messages, report));
    }

    // Step 1: re-compact the oldest oversized tool rows to the floor.
    let floor = CompactionPolicy {
        max_lines: 40,
        head_lines: 15,
        max_bytes: TOOL_FLOOR_BYTES,
    };
    for m in messages.iter_mut() {
        if estimated <= window {
            break;
        }
        if m.role != Role::Tool || m.content.len() <= TOOL_FLOOR_BYTES {
            continue;
        }
        let before = row_tokens(m);
        let c = compact_output(&m.content, &floor);
        if c.truncated {
            m.content = c.text;
            let after = row_tokens(m);
            estimated = estimated.saturating_sub(before.saturating_sub(after));
            report.compacted_rows += 1;
        }
    }
    report.estimated = estimated;
    if estimated <= window {
        return Ok((messages, report));
    }

    // Step 2: drop oldest whole exchanges (user row .. next user row).
    // The final exchange is always kept: it carries the live turn and any
    // pending assistant/tool pairing the loop must settle.
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();
    let mut drop_until = 0usize;
    for k in 0..starts.len().saturating_sub(1) {
        if estimated <= window {
            break;
        }
        let end = starts[k + 1];
        let ex: u32 = messages[starts[k]..end].iter().map(row_tokens).sum();
        estimated = estimated.saturating_sub(ex);
        report.dropped_rows += (end - starts[k]) as u32;
        drop_until = end;
    }
    if let Some(&first_user) = starts.first() {
        // Drain only the dropped exchanges; rows before the first user
        // row (system preamble) are never dropped.
        if drop_until > first_user {
            messages.drain(first_user..drop_until);
        }
    }
    report.estimated = estimated;

    // Step 3: essential rows (system preamble + final exchange) still over.
    if estimated > window {
        return Err(PantheonError::new(
            "CONTEXT_OVERFLOW",
            Layer::Runtime,
            false,
            format!(
                "context is ~{estimated} tokens after trimming, window allows {window}"
            ),
            "shorten the system prompt or extension context, or use a model with a larger context window",
            "",
        ));
    }
    Ok((messages, report))
}

/// Per-row cap when rendering the compression transcript: old tool rows can
/// be far above the fit floor (they were compacted per-call, not per-fit).
pub const RENDER_ROW_CHARS: usize = 2_000;
/// Absolute cap on the rendered transcript, protecting the aux endpoint.
pub const RENDER_MAX_CHARS: usize = 100_000;
/// Summary floor: below this a summary is noise, not signal.
pub const SUMMARY_MIN_CHARS: usize = 128;

/// What one compression pass absorbed, for the ledger event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompressionReport {
    /// Exchanges absorbed into the summary.
    pub exchanges: u32,
    /// Rows removed (replaced by one summary row).
    pub rows: u32,
    /// Rendered transcript size in chars.
    pub chars_before: u32,
    /// Summary row size in chars.
    pub chars_after: u32,
}

/// Render rows `range` as role-tagged text for the compression model,
/// capping each row at `RENDER_ROW_CHARS` and the whole render at
/// `RENDER_MAX_CHARS` (truncation returns what fits).
pub fn render_exchanges(messages: &[Message], range: std::ops::Range<usize>) -> String {
    let mut out = String::new();
    for m in &messages[range] {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let mut line = String::from(role);
        line.push_str(": ");
        let content: String = m.content.chars().take(RENDER_ROW_CHARS).collect();
        line.push_str(&content);
        if m.content.chars().count() > RENDER_ROW_CHARS {
            line.push_str(" [...]");
        }
        line.push('\n');
        // Bound before appending: a row must never push the render past
        // the cap (checking after would overshoot by up to a full row).
        if out.len() + line.len() > RENDER_MAX_CHARS {
            out.push_str("[... render cap ...]\n");
            break;
        }
        out.push_str(&line);
    }
    out
}

/// Overflow-triggered compression of the oldest exchanges via the host's
/// compression aux model — the model-assisted step BETWEEN deterministic
/// tool compaction and deterministic dropping (exec itself never talks to
/// a model; the host passes a `ContextCompressor` implementation).
///
/// - Under budget: `Ok(None)`, nothing touched.
/// - Over budget: oldest exchanges (never the final one, never the system
///   preamble) are summarized into one memory-tier note spliced where the
///   absorbed exchanges stood. The caller re-runs [`fit_to_window`] after;
///   any remainder (or a compressor error) falls back to deterministic
///   dropping.
pub fn compress_oldest(
    messages: &[Message],
    compressor: &dyn ContextCompressor,
    budget: &WindowBudget,
    run_id: &str,
) -> Result<Option<(Vec<Message>, CompressionReport)>, PantheonError> {
    let window = budget.usable();
    let estimated = estimate_messages(messages);
    if estimated <= window {
        return Ok(None);
    }

    // Exchange boundaries: a user row through the row before the next user
    // row. The final exchange is the live turn — never absorbed. Anything
    // before the first user row is the system preamble — never absorbed.
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();
    if starts.len() < 2 {
        return Ok(None); // no droppable exchange to compress
    }

    // Absorb oldest exchanges until the material covers the overflow (plus
    // a tenth of the window as slack so one pass usually suffices).
    let needed = estimated.saturating_sub(window.saturating_sub(window / 10));
    let mut chunk_end = starts[0];
    let mut chunk_tokens = 0u32;
    let mut exchanges = 0u32;
    for k in 0..starts.len() - 1 {
        if chunk_tokens >= needed && exchanges > 0 {
            break;
        }
        let end = starts[k + 1];
        if chunk_tokens > 0 && render_exchanges(messages, starts[0]..end).len() >= RENDER_MAX_CHARS
        {
            break; // input cap: leave the rest to the deterministic drop
        }
        let ex: u32 = messages[chunk_end..end].iter().map(row_tokens).sum();
        chunk_tokens += ex;
        chunk_end = end;
        exchanges += 1;
    }
    if exchanges == 0 {
        return Ok(None);
    }

    let range = starts[0]..chunk_end;
    let transcript = render_exchanges(messages, range.clone());
    if transcript.trim().is_empty() {
        return Ok(None);
    }

    // Summary budget: an eighth of the absorbed material, floored so the
    // note carries real signal, never more than half the input.
    let target_chars = ((chunk_tokens as usize / 8) * 4)
        .max(SUMMARY_MIN_CHARS)
        .min(transcript.len() / 2)
        .max(SUMMARY_MIN_CHARS);
    let req = CompressionRequest {
        run_id: run_id.to_string(),
        transcript,
        target_chars,
    };
    let result = compressor.compress(&req)?;
    let summary = result.summary.trim();
    if summary.is_empty() {
        return Err(PantheonError::new(
            "COMPRESSION_EMPTY",
            Layer::Execution,
            true,
            "compression model returned an empty summary".to_string(),
            "the deterministic fit still runs; check the [compression] endpoint",
            "",
        ));
    }

    // Memory-tier context: informative, never authoritative. Providers
    // render the envelope so the model sees the trust inline.
    let note = Message::system(format!(
        "<compressed_context>\n{summary}\n</compressed_context>"
    ))
    .with_provenance(pantheon_api::provenance::Provenance::memory(
        "context-compression",
    ));
    let chars_after = note.content.len() as u32;
    let chars_before = messages[range.clone()]
        .iter()
        .map(|m| m.content.len() as u32)
        .sum();

    let mut out = Vec::with_capacity(messages.len());
    out.extend_from_slice(&messages[..range.start]);
    out.push(note);
    out.extend_from_slice(&messages[range.end..]);
    Ok(Some((
        out,
        CompressionReport {
            exchanges,
            rows: (range.end - range.start) as u32,
            chars_before,
            chars_after,
        },
    )))
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
