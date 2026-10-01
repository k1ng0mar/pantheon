//! Completion judging: an auxiliary model decides whether a swarm's
//! combined work actually satisfies the original task.
//!
//! The judge is deliberately narrow. It does not re-run anything and
//! does not see the world — it reads the agents' work summaries plus
//! the original task and returns a verdict: done or not, with notes.
//! Parsing is fail-closed: an unrecognized verdict means
//! `done: false`, never a silent pass.
//!
//! Two entry points:
//!
//! - [`judge_completion_with`]: run the judge through a
//!   [`JudgeTransport`]. This is what the swarm orchestrator uses.
//! - [`judge_completion`]: no transport configured. Fail-closed —
//!   returns `done: false` with a note saying to wire a transport.
//!
//! The production transport lives in the dashboard crate
//! (`SubprocessWorker`'s judge), which resolves the `[judge]` aux
//! section from config; this crate stays transport-agnostic.

/// Whether the swarm's combined work satisfies the original task.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct JudgeVerdict {
    /// True only when the judge positively recognized completion.
    pub done: bool,
    /// The judge's reasoning / what is still missing.
    pub notes: String,
}

/// One-shot transport for the judge prompt. Implemented by hosts that
/// know how to reach the configured `[judge]` aux model.
pub trait JudgeTransport: Send + Sync {
    /// Send the judge prompt; return the model's raw text verdict.
    fn judge(&self, prompt: &str) -> Result<String, String>;
}

/// Build the judge prompt from the agents' work summaries and the
/// original task. The format contract the parser expects is stated in
/// the prompt itself: the verdict line must be exactly
/// `VERDICT: done` or `VERDICT: not done`.
pub fn judge_prompt(work_summary: &str, original_task: &str) -> String {
    format!(
        "You are a strict completion judge for a multi-agent swarm.\n\
         Original task:\n{original_task}\n\n\
         Combined work produced by the swarm agents:\n{work_summary}\n\n\
         Decide whether the combined work fully satisfies the original task.\n\
         Be strict: partial work, missing pieces, or unverified claims mean not done.\n\
         Reply with exactly two parts:\n\
         - a verdict line that is exactly `VERDICT: done` or `VERDICT: not done`\n\
         - then `NOTES:` followed by your reasoning and what is still missing, if anything.\n"
    )
}

/// Parse a raw judge reply into a verdict. Fail-closed: anything that
/// does not contain an exact `VERDICT: done` line parses as not done.
pub fn parse_judge_verdict(raw: &str) -> JudgeVerdict {
    let done = raw
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("VERDICT: done"));
    let notes = raw
        .lines()
        .skip_while(|l| {
            !l.trim().eq_ignore_ascii_case("VERDICT: done")
                && !l.trim().eq_ignore_ascii_case("VERDICT: not done")
        })
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    let notes = if notes.is_empty() {
        raw.trim().to_string()
    } else {
        notes
    };
    JudgeVerdict { done, notes }
}

/// Run the completion judge through `transport`. A transport error is
/// fail-closed: `done: false` with the error recorded in notes.
pub fn judge_completion_with(
    transport: &dyn JudgeTransport,
    work_summary: &str,
    original_task: &str,
) -> JudgeVerdict {
    let prompt = judge_prompt(work_summary, original_task);
    match transport.judge(&prompt) {
        Ok(raw) => parse_judge_verdict(&raw),
        Err(e) => JudgeVerdict {
            done: false,
            notes: format!("judge transport failed: {e}"),
        },
    }
}

/// Judge with no transport configured. Fail-closed by construction:
/// `done: false` with a note pointing at `judge_completion_with`.
pub fn judge_completion(work_summary: &str, original_task: &str) -> JudgeVerdict {
    let _ = (work_summary, original_task);
    JudgeVerdict {
        done: false,
        notes: "judge transport not configured (use judge_completion_with)".to_string(),
    }
}
