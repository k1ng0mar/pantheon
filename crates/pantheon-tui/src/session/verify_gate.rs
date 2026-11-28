//! Verification gate: a completion claim with no probe in the same turn.
//!
//! The rule is the one from the verification doctrine: never claim done without
//! evidence from this session. This gate is deliberately narrow and text-only -
//! it never rewrites the answer, never retries a turn and never spends a token.
//! It reads the finished turn's transcript (the same evidence the operator can
//! see) and reports the claims that no tool call backs.
//!
//! Precision matters more than recall here: a false "unverified" card on a
//! normal conversational answer would train the operator to ignore it. So the
//! gate only fires on
//!   * a test / build / deploy claim with no command of that kind in the turn,
//!   * "should work"-style guesses,
//!   * a generic done/fixed claim in a turn that edited files but ran nothing.
//!
//! A plain "done" in a read-only, conversational turn is left alone.

use super::{BlockKind, TranscriptBlock};

/// one claim the turn cannot back
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// the sentence that made the claim, clipped
    pub sentence: String,
    /// why it is unbacked, phrased for the operator
    pub reason: String,
}

/// at most this many findings per turn; a wall of cards is noise
const MAX_FINDINGS: usize = 3;
/// a claim sentence shown in the card never runs past this
const SENTENCE_CAP: usize = 160;

/// Tools that change the workspace. A generic "done" claim in a turn that used
/// one of these without running anything afterwards is the case worth flagging.
fn is_mutating(name: &str) -> bool {
    matches!(
        name,
        "write_file" | "edit_file" | "apply_patch" | "notebook_edit" | "skill_manage"
    )
}

/// The tool evidence a turn accumulated, reduced to what the rules ask about.
#[derive(Debug, Default)]
struct Evidence {
    any_tool: bool,
    mutating: bool,
    /// lowercased shell commands run this turn
    commands: Vec<String>,
}

impl Evidence {
    fn any_command_contains(&self, needles: &[&str]) -> bool {
        self.commands
            .iter()
            .any(|c| needles.iter().any(|n| c.contains(n)))
    }
}

/// Check the finished turn in `blocks` (everything after the last user
/// message). Empty when the turn has no final assistant answer or nothing to
/// flag.
pub fn check(blocks: &[TranscriptBlock]) -> Vec<Finding> {
    let start = blocks
        .iter()
        .rposition(|b| matches!(b.kind, BlockKind::UserMessage(_)))
        .map(|i| i + 1)
        .unwrap_or(0);
    let turn = &blocks[start..];
    if turn.is_empty() {
        return Vec::new();
    }
    let Some(final_text) = turn.iter().rev().find_map(|b| match &b.kind {
        BlockKind::AssistantMessage(t) if !t.trim().is_empty() => Some(t.as_str()),
        _ => None,
    }) else {
        return Vec::new();
    };
    let mut ev = Evidence::default();
    for b in turn {
        if let BlockKind::ToolCall { name, args, .. } = &b.kind {
            ev.any_tool = true;
            if is_mutating(name) {
                ev.mutating = true;
            }
            if name == "shell" {
                ev.commands.push(args.to_lowercase());
            }
        }
    }
    check_text(final_text, &ev)
}

fn check_text(text: &str, ev: &Evidence) -> Vec<Finding> {
    let text = strip_code_blocks(text);
    let mut out: Vec<Finding> = Vec::new();
    for sentence in sentences(&text) {
        let low = sentence.to_lowercase();
        if is_hedged(&low) {
            continue;
        }
        let Some(reason) = classify(&low, ev) else {
            continue;
        };
        out.push(Finding {
            sentence: clip(&sentence, SENTENCE_CAP),
            reason,
        });
        if out.len() >= MAX_FINDINGS {
            break;
        }
    }
    out
}

/// Fenced code and inline code say what the code is, not what was verified.
/// Drop fences entirely so a pasted test name cannot trigger a claim.
fn strip_code_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// split on sentence punctuation and line breaks; trimmed, empties dropped
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '.' || ch == '!' || ch == '?' || ch == '\n' {
            let s = cur.trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    let s = cur.trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out
}

/// a claim the turn already disclaims needs nothing: honesty is the fix
fn is_hedged(low: &str) -> bool {
    const HEDGES: &[&str] = &[
        "not verified",
        "unverified",
        "couldn't verify",
        "could not verify",
        "didn't run",
        "did not run",
        "haven't tested",
        "have not tested",
        "not tested",
        "not yet run",
    ];
    HEDGES.iter().any(|h| low.contains(h))
}

fn has_any(low: &str, words: &[&str]) -> bool {
    words.iter().any(|w| low.contains(w))
}

/// first matching class wins, so one sentence yields one finding
fn classify(low: &str, ev: &Evidence) -> Option<String> {
    // Test claims: a pass word is required, so "add tests" is not a claim.
    if has_any(low, &["test", "suite"])
        && has_any(
            low,
            &["pass", "passed", "green", "succeed", "success", "clean"],
        )
        && !ev.any_command_contains(&["test", "pytest", "vitest", "jest", "check"])
    {
        return Some("no test command ran in this turn".into());
    }
    // Build claims.
    if has_any(low, &["build", "compile", "compiles", "typechecks", "type-checks"])
        && has_any(
            low,
            &["pass", "passed", "green", "succeed", "clean", "works", "fine"],
        )
        // `cargo test` is a test, not a build check: the probe has to match
        // the claim, or one command would vouch for every kind of result.
        && !ev.any_command_contains(&["build", "compile", "check", "tsc", "make"])
    {
        return Some("no build or compile command ran in this turn".into());
    }
    // Deploy claims: a claim of "live"/"deployed"/"shipped" needs the command.
    if has_any(
        low,
        &["deployed", "shipped", "released", "published", "live at", "is live"],
    ) && !ev.any_command_contains(&["deploy", "publish", "release", "push", "gh "])
    {
        return Some("no deploy or publish command ran in this turn".into());
    }
    // "Should work" is a guess by construction.
    if low.contains("should work") || low.contains("should be working") {
        return Some("\"should work\" is a guess, not evidence".into());
    }
    // Generic completion claims: only when the turn changed files and ran
    // nothing. A read-only explanation is allowed to say "done".
    if ev.mutating
        && ev.commands.is_empty()
        && has_any(
            low,
            &[
                " done",
                "done.",
                "fixed",
                "resolved",
                "implemented",
                "it works",
                "works now",
                "working now",
                "complete",
                "completed",
                "no errors",
                "all good",
            ],
        )
    {
        return Some("files changed but no command ran to check them".into());
    }
    None
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.trim().to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(text: &str, tools: Vec<(&str, &str)>) -> Vec<TranscriptBlock> {
        let mut blocks = vec![
            TranscriptBlock {
                kind: BlockKind::UserMessage("do the thing".into()),
            },
            TranscriptBlock {
                kind: BlockKind::AssistantMessage(text.into()),
            },
        ];
        for (name, args) in tools {
            blocks.insert(
                1,
                TranscriptBlock {
                    kind: BlockKind::ToolCall {
                        name: name.into(),
                        args: args.into(),
                        ok: Some(true),
                        call_id: format!("c-{name}"),
                        started: None,
                        duration: None,
                        error: None,
                        tokens: None,
                        expanded: false,
                        output: Vec::new(),
                    },
                },
            );
        }
        blocks
    }

    #[test]
    fn test_claim_without_a_test_command_is_flagged() {
        let f = check(&turn("Tests pass now.", vec![("edit_file", "src/lib.rs")]));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].reason, "no test command ran in this turn");
        assert!(f[0].sentence.contains("Tests pass"));
    }

    #[test]
    fn test_claim_with_the_test_command_is_clean() {
        let f = check(&turn(
            "All 376 tests pass.",
            vec![("shell", "cargo test -p pantheon-tui --lib")],
        ));
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn build_and_deploy_claims_need_their_own_probe() {
        let f = check(&turn("The build is green.", vec![("shell", "cargo test")]));
        assert_eq!(f[0].reason, "no build or compile command ran in this turn");
        let f = check(&turn("Deployed to production.", vec![("shell", "cargo build")]));
        assert_eq!(f[0].reason, "no deploy or publish command ran in this turn");
    }

    #[test]
    fn generic_done_needs_an_edit_and_no_command() {
        // read-only conversational turn: not flagged
        assert!(check(&turn("Done.", vec![])).is_empty());
        assert!(check(&turn("Done.", vec![("read_file", "src/lib.rs")])).is_empty());
        // read + explained after checking: command evidence exists
        assert!(check(&turn("Fixed it.", vec![("shell", "cargo test")])).is_empty());
        // edited with nothing run: flagged
        let f = check(&turn("Fixed the parser.", vec![("edit_file", "src/p.rs")]));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].reason, "files changed but no command ran to check them");
    }

    #[test]
    fn should_work_is_always_flagged_and_hedges_are_not() {
        let f = check(&turn("It should work now.", vec![]));
        assert_eq!(f[0].reason, "\"should work\" is a guess, not evidence");
        assert!(check(&turn("Deployed, not verified.", vec![])).is_empty());
        assert!(check(&turn("I have not tested this yet.", vec![])).is_empty());
    }

    #[test]
    fn code_blocks_do_not_trigger_claims() {
        let text = "Here is the patch:\n```\n// tests pass\n```\n";
        assert!(check(&turn(text, vec![])).is_empty());
    }

    #[test]
    fn findings_are_capped_and_clipped() {
        let text = format!(
            "Tests pass. The build is green. Deployed to prod. It should work. {}",
            "x".repeat(400)
        );
        let f = check(&turn(&text, vec![]));
        assert_eq!(f.len(), MAX_FINDINGS);
        assert!(f.iter().all(|x| x.sentence.chars().count() <= SENTENCE_CAP));
    }

    #[test]
    fn only_the_latest_turn_is_checked() {
        let mut blocks = vec![
            TranscriptBlock {
                kind: BlockKind::UserMessage("old turn".into()),
            },
            TranscriptBlock {
                kind: BlockKind::AssistantMessage("Tests pass.".into()),
            },
        ];
        blocks.extend(turn("Nothing to report.", vec![]));
        assert!(
            check(&blocks).is_empty(),
            "an old unbacked claim must not fire on a later turn"
        );
    }
}
