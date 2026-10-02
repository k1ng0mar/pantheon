//! Shared preamble for pulling the answer line out of a chatty aux-model
//! reply: drop code-fence lines, then keep the non-empty trimmed lines.
//!
//! Both the judge (`judge.rs`) and the adversarial verifier (`verify.rs`)
//! grew their own copy of this preamble and DIVERGED in the matcher that
//! picks the answer line - so the preamble lives here, but the two matchers
//! stay separate on purpose. Unifying the matchers would change untested
//! behavior: for trailing prose *after* the answer line, the verifier picks
//! the `ANSWER`-prefixed line while the judge falls back to the last line
//! (its strict matcher only fires on a bare `ANSWER` or an `answer:`-prefixed
//! line). No existing test discriminates between them - every test in both
//! suites puts the answer on the last line, where both matchers agree - so
//! the two-matcher split is the honest outcome: both matchers are frozen
//! as-is rather than silently "fixed" into one.

/// Code-fence-stripped, non-empty, trimmed lines of a model reply.
///
/// The fence filter uses `trim_start`, so indented fences (common in chatty
/// model output) are dropped too.
pub(crate) fn non_empty_unfenced_lines(raw: &str) -> Vec<String> {
    raw.lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}
