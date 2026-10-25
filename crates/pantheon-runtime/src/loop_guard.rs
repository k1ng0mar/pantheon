//! Loop-hygiene gate: the dsh `repeat-tool-reminder` guard, expressed with
//! Pantheon's own deny-with-reason semantics (plan doc §10).
//!
//! A model that issues the same tool call with the same arguments three
//! times in a row is stuck: each identical call burns time and tokens and
//! changes nothing. On the threshold call this gate refuses with a
//! model-visible reason - an ordinary tool-shaped refusal, exactly like the
//! extension gate's - so the model can change approach, adjust arguments,
//! or finish. Nothing is blocked permanently: the counter resets once the
//! call differs (or the turn ends, since the guard lives one turn).
//!
//! Comparison is exact on (tool, args) - no hashing, so a hash collision can
//! never deny a call that differs. State is per-turn by construction: each
//! user message is a fresh budget, which keeps a legitimate retry pattern
//! ("read file, then read it again after editing") from tripping across
//! turns while still stopping a same-turn spin.

const DEFAULT_THRESHOLD: u32 = 3;
/// Args compared in full; this cap only bounds one denial's error text.
const ARGS_SNIPPET: usize = 160;

#[derive(Default)]
struct TurnState {
    last: Option<(String, String)>,
    count: u32,
}

pub struct LoopGuard {
    state: std::sync::Mutex<TurnState>,
    threshold: u32,
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopGuard {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(TurnState::default()),
            threshold: DEFAULT_THRESHOLD,
        }
    }

    /// `Ok(())` proceeds; `Err(reason)` is the model-visible refusal text.
    pub fn admit(&self, tool: &str, args: &str) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let repeat = state
            .last
            .as_ref()
            .is_some_and(|(last_tool, last_args)| last_tool == tool && last_args == args);
        if repeat {
            state.count += 1;
        } else {
            state.last = Some((tool.to_string(), args.to_string()));
            state.count = 1;
        }
        if state.count < self.threshold {
            return Ok(());
        }
        let count = state.count;
        // Fresh budget after the refusal: the next differing call starts over.
        state.last = None;
        state.count = 0;
        let snippet: String = args.chars().take(ARGS_SNIPPET).collect();
        Err(format!(
            "loop guard: identical call to `{tool}` repeated {count} times in a row (args: {snippet}). \
             Change approach: gather different information, adjust the arguments, or finish the task."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_distinct_calls_and_first_repeats() {
        let guard = LoopGuard::new();
        assert!(guard.admit("read_file", r#"{"path":"a"}"#).is_ok());
        assert!(guard.admit("read_file", r#"{"path":"b"}"#).is_ok());
        assert!(guard.admit("read_file", r#"{"path":"a"}"#).is_ok());
        assert!(guard.admit("read_file", r#"{"path":"a"}"#).is_ok());
        // The third consecutive identical `a` denies (the contract
        // `denies_on_third_consecutive_identical_call` pins); the test
        // formerly asserted a fifth Ok, which that contract forbids.
        let err = guard.admit("read_file", r#"{"path":"a"}"#).unwrap_err();
        assert!(err.contains("3 times"));
    }

    #[test]
    fn denies_on_third_consecutive_identical_call() {
        let guard = LoopGuard::new();
        let args = r#"{"path":"a"}"#;
        assert!(guard.admit("read_file", args).is_ok());
        assert!(guard.admit("read_file", args).is_ok());
        let err = guard.admit("read_file", args).unwrap_err();
        assert!(err.contains("3 times"));
        assert!(err.contains("read_file"));
    }

    #[test]
    fn budget_resets_after_a_denial_and_after_a_change() {
        let guard = LoopGuard::new();
        let args = r#"{"path":"a"}"#;
        for _ in 0..3 {
            let _ = guard.admit("read_file", args);
        }
        // A different call resets the streak.
        assert!(guard.admit("list_dir", "{}").is_ok());
        assert!(guard.admit("read_file", args).is_ok());
        assert!(guard.admit("read_file", args).is_ok());
        assert!(guard.admit("read_file", args).is_err());
    }
}
