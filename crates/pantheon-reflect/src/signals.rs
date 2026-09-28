//! Ledger-native signal extraction.
//!
//! No transcript re-reads: every signal is derived from structured ledger
//! events, which is cheaper and more precise than asking a model to
//! re-read conversations. All thresholds are named constants.

use pantheon_api::error::PantheonError;
use pantheon_api::events::Event;
use pantheon_storage::Ledger;
use std::collections::HashMap;

/// A turn that produced a signal: the run it belongs to and the turn.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TurnRef {
    pub run_id: String,
    pub turn_id: String,
}

/// One learnable observation from the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The same ordered tool sequence completed successfully this many
    /// times. A candidate for a reusable skill.
    RepeatedSequence {
        tools: Vec<String>,
        hits: Vec<TurnRef>,
    },
    /// The operator steered a running turn: a mid-turn correction, i.e. the
    /// agent was heading somewhere the user didn't want.
    UserCorrection { text: String, at: TurnRef },
    /// The operator denied an approval scope.
    ApprovalDenied { scope: String, at: TurnRef },
    /// The same tool failed repeatedly: a candidate "check preconditions"
    /// lesson.
    RepeatedFailure {
        tool: String,
        count: usize,
        at: Vec<TurnRef>,
    },
    /// Several steered turns share one preference keyword: a standing
    /// behavioral preference, i.e. a persona-note candidate. The keyword
    /// vocabulary is a fixed curated list on purpose — persona inference
    /// from free text is heuristic, so it stays narrow, and every
    /// proposal it produces still needs human approval.
    RepeatedPreference {
        topic: String,
        keyword: String,
        hits: Vec<TurnRef>,
    },
}

/// Minimum successful repetitions before a tool sequence becomes a skill
/// candidate. Below this it's a habit, not a pattern.
pub const MIN_SEQUENCE_REPEATS: usize = 3;
/// Minimum tools in a sequence to be worth a skill. Single-tool repeats
/// are usually just the agent doing its job.
pub const MIN_SEQUENCE_LEN: usize = 2;
/// Minimum failures of one tool before a preconditions lesson fires.
pub const MIN_FAILURE_REPEATS: usize = 2;
/// Preference keywords: (matched word, persona topic). Curated and fixed;
/// see [`Signal::RepeatedPreference`].
pub const PREFERENCE_KEYWORDS: &[(&str, &str)] = &[
    ("concise", "brevity"),
    ("brief", "brevity"),
    ("terse", "brevity"),
    ("verbose", "thoroughness"),
    ("detailed", "thoroughness"),
    ("thorough", "thoroughness"),
    ("explain", "explanation"),
    ("confirm", "confirmation"),
    ("proactive", "proactivity"),
];
/// Minimum steered turns sharing one preference keyword before a persona
/// note fires.
pub const MIN_PREFERENCE_REPEATS: usize = 3;
/// ... and they must span this many runs: one frustrated afternoon is a
/// mood, not a preference.
pub const MIN_PREFERENCE_RUNS: usize = 2;
/// Cap on runs scanned per pass: reflection is bounded work.
pub const MAX_RUNS_SCANNED: usize = 10_000;

/// Collect signals from ledger events at or after `since_ms`.
pub fn collect(ledger: &Ledger, since_ms: i64) -> Result<Vec<Signal>, PantheonError> {
    let mut signals = Vec::new();
    // turn key -> ordered tool names started in that turn
    let mut turn_tools: HashMap<(String, String), Vec<String>> = HashMap::new();
    // turn key -> did the turn complete successfully
    let mut turn_ok: HashMap<(String, String), bool> = HashMap::new();
    let mut failures: HashMap<String, Vec<TurnRef>> = HashMap::new();
    // (steering text, where) for preference grouping below.
    let mut corrections: Vec<(String, TurnRef)> = Vec::new();

    for listing in ledger.list_runs(MAX_RUNS_SCANNED)? {
        let run_id = listing.0.clone();
        let mut current_turn: Option<String> = None;
        for entry in ledger.replay(&run_id)? {
            if entry.ts_ms < since_ms {
                continue;
            }
            match &entry.event {
                Event::TurnStarted { turn_id, .. } => {
                    current_turn = Some(turn_id.clone());
                }
                Event::ToolStarted { tool, .. } => {
                    if let Some(t) = &current_turn {
                        turn_tools
                            .entry((run_id.clone(), t.clone()))
                            .or_default()
                            .push(tool.clone());
                    }
                }
                Event::TurnCompleted { turn_id, .. } => {
                    turn_ok.insert((run_id.clone(), turn_id.clone()), true);
                }
                Event::TurnFailed { turn_id, .. } => {
                    turn_ok.insert((run_id.clone(), turn_id.clone()), false);
                    // Attribute the failure to the last tool started in
                    // the turn, if any: the most likely culprit.
                    if let Some(tools) = turn_tools.get(&(run_id.clone(), turn_id.clone())) {
                        if let Some(tool) = tools.last() {
                            failures.entry(tool.clone()).or_default().push(TurnRef {
                                run_id: run_id.clone(),
                                turn_id: turn_id.clone(),
                            });
                        }
                    }
                }
                Event::SteeringProvided { text, .. } => {
                    let at = TurnRef {
                        run_id: run_id.clone(),
                        turn_id: current_turn.clone().unwrap_or_default(),
                    };
                    corrections.push((text.clone(), at.clone()));
                    signals.push(Signal::UserCorrection {
                        text: text.clone(),
                        at,
                    });
                }
                Event::ApprovalDenied { scope, .. } => {
                    let at = TurnRef {
                        run_id: run_id.clone(),
                        turn_id: current_turn.clone().unwrap_or_default(),
                    };
                    signals.push(Signal::ApprovalDenied {
                        scope: scope.clone(),
                        at,
                    });
                }
                _ => {}
            }
        }
    }

    // Group successful turns by their exact tool sequence.
    let mut seq_hits: HashMap<Vec<String>, Vec<TurnRef>> = HashMap::new();
    for ((run_id, turn_id), tools) in &turn_tools {
        if tools.len() < MIN_SEQUENCE_LEN {
            continue;
        }
        if turn_ok.get(&(run_id.clone(), turn_id.clone())) != Some(&true) {
            continue;
        }
        seq_hits.entry(tools.clone()).or_default().push(TurnRef {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        });
    }
    let mut seqs: Vec<(Vec<String>, Vec<TurnRef>)> = seq_hits
        .into_iter()
        .filter(|(_, hits)| hits.len() >= MIN_SEQUENCE_REPEATS)
        .collect();
    // Deterministic order: most hits first, then lexicographic.
    seqs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (tools, hits) in seqs {
        signals.push(Signal::RepeatedSequence { tools, hits });
    }

    let mut fails: Vec<(String, Vec<TurnRef>)> = failures
        .into_iter()
        .filter(|(_, at)| at.len() >= MIN_FAILURE_REPEATS)
        .collect();
    fails.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (tool, at) in fails {
        signals.push(Signal::RepeatedFailure {
            count: at.len(),
            tool,
            at,
        });
    }

    // Standing preferences: corrections sharing one preference keyword,
    // grouped by (keyword, topic). Deterministic order: topic, keyword.
    let mut pref_hits: HashMap<(&str, &str), Vec<TurnRef>> = HashMap::new();
    for (text, at) in &corrections {
        let ws = words(text);
        for (word, topic) in PREFERENCE_KEYWORDS {
            if ws.iter().any(|w| w == word) {
                pref_hits.entry((word, topic)).or_default().push(at.clone());
            }
        }
    }
    let mut prefs: Vec<((&str, &str), Vec<TurnRef>)> = pref_hits
        .into_iter()
        .filter(|(_, hits)| {
            hits.len() >= MIN_PREFERENCE_REPEATS
                && hits
                    .iter()
                    .map(|h| &h.run_id)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    >= MIN_PREFERENCE_RUNS
        })
        .collect();
    prefs.sort_by(|a, b| a.0 .1.cmp(b.0 .1).then(a.0 .0.cmp(b.0 .0)));
    for ((keyword, topic), hits) in prefs {
        signals.push(Signal::RepeatedPreference {
            topic: topic.to_string(),
            keyword: keyword.to_string(),
            hits,
        });
    }

    Ok(signals)
}

/// Lowercase word list of a text, for whole-word keyword matching.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

// Small deterministic invariant tests only.
#[cfg(test)]
mod tests {
    use super::*;

    // Thresholds are compile-time configuration: assert them at compile
    // time rather than in a test so a bad value fails the build.
    const _: () = assert!(MIN_SEQUENCE_REPEATS >= 2);
    const _: () = assert!(MIN_SEQUENCE_LEN >= 2);
    const _: () = assert!(MIN_FAILURE_REPEATS >= 2);
}
