//! Unified ledger scan: one pass over recent runs yields both behavior
//! signals (proposal mining) and memory candidates (promotion).
//!
//! Replaces the old duplicated scans in `pantheon-reflect::signals` and
//! `pantheon-consolidate::stage`. The scan is bounded (`max_runs` runs,
//! events at or after `since_ms`) and deterministic: the same ledger
//! state always yields the same signals in the same order.

use pantheon_api::capability::Policy;
use pantheon_api::error::PantheonError;
use pantheon_api::events::Event;
use pantheon_memory::{LayerKind, MemoryBackend, Recalled};
use pantheon_storage::Ledger;
use std::collections::HashMap;

/// Knobs for the unified scan.
#[derive(Debug, Clone)]
pub struct CollectPolicy {
    /// A tool sequence needs this many successful repetitions to count.
    pub min_sequence_repeats: usize,
    /// A tool needs this many attributed failures to count.
    pub min_failure_repeats: usize,
    /// A preference keyword needs this many hits to count.
    pub min_preference_hits: usize,
}

impl Default for CollectPolicy {
    fn default() -> Self {
        Self {
            min_sequence_repeats: 3,
            min_failure_repeats: 3,
            min_preference_hits: 3,
        }
    }
}

/// Minimum tool calls in a turn for the turn to count as a "sequence".
const MIN_SEQUENCE_LEN: usize = 2;

// `TurnRef` lives at the API layer (see `pantheon_api::nightly`): it is
// part of the `Proposal` DTO shared with providers. Re-exported here so
// `pantheon_nightly::TurnRef` keeps resolving.
pub use pantheon_api::nightly::TurnRef;

/// Behavior signals mined from the ledger.
#[derive(Debug, Clone)]
pub enum Signal {
    /// A tool sequence succeeded in N turns - skill material.
    RepeatedSequence {
        tools: Vec<String>,
        hits: Vec<TurnRef>,
    },
    /// The user steered a turn mid-flight.
    UserCorrection { text: String, at: TurnRef },
    /// An approval was denied.
    ApprovalDenied { scope: String, at: TurnRef },
    /// A tool failed repeatedly - precondition-lesson material.
    RepeatedFailure {
        tool: String,
        count: usize,
        at: Vec<TurnRef>,
    },
    /// A preference keyword recurred across turns - persona material.
    RepeatedPreference {
        topic: &'static str,
        keyword: &'static str,
        hits: Vec<TurnRef>,
    },
}

/// Where a memory candidate came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Source {
    pub run_id: String,
    pub ts_ms: i64,
}

/// What kind of memory a candidate wants to become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateKind {
    /// A steering correction from a user - standing guidance.
    Steering,
    /// Something the agent wrote at session scope worth keeping.
    SessionMemory,
}

impl CandidateKind {
    pub fn label(&self) -> &'static str {
        match self {
            CandidateKind::Steering => "steering",
            CandidateKind::SessionMemory => "session-memory",
        }
    }
}

/// A candidate durable memory with provenance.
#[derive(Debug, Clone)]
pub struct MemoryCandidate {
    pub text: String,
    pub kind: CandidateKind,
    pub sources: Vec<Source>,
}

/// What one ledger scan produced.
pub struct ScanResult {
    /// How many runs were actually walked.
    pub runs_scanned: usize,
    pub signals: Vec<Signal>,
    pub candidates: Vec<MemoryCandidate>,
    /// Per-tool invocation health over the scanned window: total calls
    /// (`ToolStarted`) and errors attributed via failed turns (the same
    /// attribution as the `RepeatedFailure` signal). Feeds the nightly
    /// repair loop's broken-tool detection.
    pub tool_stats: Vec<crate::repair_targets::ToolCallStats>,
}

/// One pass over the ledger: behavior signals AND memory candidates.
///
/// Events before `since_ms` are skipped; at most `max_runs` runs are
/// scanned (most recent first). Memory candidates also need the memory
/// backend: the ledger only carries memory-write keys, so the value is
/// read back by exact (layer, namespace, key) - never a fuzzy neighbor.
pub fn collect(
    ledger: &Ledger,
    backend: &dyn MemoryBackend,
    policy: &Policy,
    collect_policy: &CollectPolicy,
    since_ms: i64,
    max_runs: usize,
) -> Result<ScanResult, PantheonError> {
    let mut signals = Vec::new();
    let mut runs_scanned = 0usize;
    let mut candidates: HashMap<(String, CandidateKindKey), Vec<Source>> = HashMap::new();
    let mut candidate_texts: HashMap<(String, CandidateKindKey), String> = HashMap::new();

    // turn key -> ordered tool names started in that turn
    let mut turn_tools: HashMap<(String, String), Vec<String>> = HashMap::new();
    // tool name -> total invocations started in the window
    let mut tool_calls: HashMap<String, u64> = HashMap::new();
    // turn key -> did the turn complete successfully
    let mut turn_ok: HashMap<(String, String), bool> = HashMap::new();
    // turn key -> when the turn started (for signal recency)
    let mut turn_ts: HashMap<(String, String), i64> = HashMap::new();
    let mut failures: HashMap<String, Vec<TurnRef>> = HashMap::new();
    // (steering text, where) for preference grouping below.
    let mut corrections: Vec<(String, TurnRef)> = Vec::new();

    for listing in ledger.list_runs(max_runs)? {
        let run_id = listing.0.clone();
        runs_scanned += 1;
        let mut current_turn: Option<String> = None;
        for entry in ledger.replay(&run_id)? {
            if entry.ts_ms < since_ms {
                continue;
            }
            match &entry.event {
                Event::TurnStarted { turn_id, .. } => {
                    current_turn = Some(turn_id.clone());
                    turn_ts
                        .entry((run_id.clone(), turn_id.clone()))
                        .or_insert(entry.ts_ms);
                }
                Event::ToolStarted { tool, .. } => {
                    *tool_calls.entry(tool.clone()).or_default() += 1;
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
                                ts_ms: entry.ts_ms,
                            });
                        }
                    }
                }
                Event::SteeringProvided { text, .. } => {
                    let at = TurnRef {
                        run_id: run_id.clone(),
                        turn_id: current_turn.clone().unwrap_or_default(),
                        ts_ms: entry.ts_ms,
                    };
                    corrections.push((text.clone(), at.clone()));
                    signals.push(Signal::UserCorrection {
                        text: text.clone(),
                        at: at.clone(),
                    });
                    push_candidate(
                        &mut candidates,
                        &mut candidate_texts,
                        CandidateKind::Steering,
                        text.clone(),
                        Source {
                            run_id: run_id.clone(),
                            ts_ms: entry.ts_ms,
                        },
                    );
                }
                Event::RunProgress { detail, .. } => {
                    if let Some((layer_name, ns, key)) = parse_memory_write(detail) {
                        // Only session-scoped writes are consolidation
                        // material; Agent/Project rows are already durable.
                        if parse_layer(&layer_name) != Some(LayerKind::TaskSession) {
                            continue;
                        }
                        // Read the value back from the store: the ledger
                        // only carries the key. Recall with the key's
                        // tokens as the query, then take the exact
                        // (layer, namespace, key) hit - never a fuzzy
                        // neighbor. A missing record (expired,
                        // forgotten) is not a candidate: we never invent
                        // text.
                        if let Ok(hits) = backend.recall(
                            policy,
                            &[ns.as_str()],
                            &[LayerKind::TaskSession],
                            &key_query(&key),
                            10,
                        ) {
                            let exact = hits.iter().find(|h: &&Recalled| {
                                h.record.layer == LayerKind::TaskSession
                                    && h.record.namespace == ns
                                    && h.record.key == key
                            });
                            if let Some(hit) = exact {
                                let text = hit.record.value.trim();
                                if !text.is_empty() {
                                    push_candidate(
                                        &mut candidates,
                                        &mut candidate_texts,
                                        CandidateKind::SessionMemory,
                                        text.to_string(),
                                        Source {
                                            run_id: run_id.clone(),
                                            ts_ms: entry.ts_ms,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
                Event::ApprovalDenied { scope, .. } => {
                    let at = TurnRef {
                        run_id: run_id.clone(),
                        turn_id: current_turn.clone().unwrap_or_default(),
                        ts_ms: entry.ts_ms,
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
            ts_ms: turn_ts
                .get(&(run_id.clone(), turn_id.clone()))
                .copied()
                .unwrap_or(0),
        });
    }
    let mut seqs: Vec<(Vec<String>, Vec<TurnRef>)> = seq_hits
        .into_iter()
        .filter(|(_, hits)| hits.len() >= collect_policy.min_sequence_repeats)
        .collect();
    // Deterministic order: most hits first, then lexicographic.
    seqs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (tools, hits) in seqs {
        signals.push(Signal::RepeatedSequence { tools, hits });
    }

    // Error counts per tool for the repair loop, captured before the
    // `failures` map is consumed below.
    let tool_errors: HashMap<String, u64> = failures
        .iter()
        .map(|(k, v)| (k.clone(), v.len() as u64))
        .collect();
    let mut fails: Vec<(String, Vec<TurnRef>)> = failures
        .into_iter()
        .filter(|(_, at)| at.len() >= collect_policy.min_failure_repeats)
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
        .filter(|(_, hits)| hits.len() >= collect_policy.min_preference_hits)
        .collect();
    prefs.sort_by(|a, b| (a.0 .1, a.0 .0).cmp(&(b.0 .1, b.0 .0)));
    for ((keyword, topic), hits) in prefs {
        signals.push(Signal::RepeatedPreference {
            topic,
            keyword,
            hits,
        });
    }

    // Memory candidates, deterministic order: most sources first, then
    // (kind, normalized text).
    let mut out: Vec<MemoryCandidate> = candidates
        .into_iter()
        .map(|((norm, kind_key), sources)| MemoryCandidate {
            text: candidate_texts
                .remove(&(norm, kind_key.clone()))
                .unwrap_or_default(),
            kind: kind_key.to_kind(),
            sources,
        })
        .collect();
    out.sort_by(|a, b| {
        b.sources
            .len()
            .cmp(&a.sources.len())
            .then(a.kind.label().cmp(b.kind.label()))
            .then(normalize(&a.text).cmp(&normalize(&b.text)))
    });

    // Per-tool invocation health for the repair loop: total calls plus
    // errors attributed via failed turns (the same attribution as the
    // `RepeatedFailure` signal).
    let mut tool_stats: Vec<crate::repair_targets::ToolCallStats> = tool_calls
        .iter()
        .map(|(name, calls)| crate::repair_targets::ToolCallStats {
            name: name.clone(),
            calls: *calls,
            errors: tool_errors.get(name).copied().unwrap_or(0),
        })
        .collect();
    tool_stats.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(ScanResult {
        runs_scanned,
        signals,
        candidates: out,
        tool_stats,
    })
}

/// Hashable stand-in for [`CandidateKind`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CandidateKindKey {
    Steering,
    SessionMemory,
}

impl CandidateKindKey {
    fn to_kind(&self) -> CandidateKind {
        match self {
            CandidateKindKey::Steering => CandidateKind::Steering,
            CandidateKindKey::SessionMemory => CandidateKind::SessionMemory,
        }
    }
}

fn push_candidate(
    candidates: &mut HashMap<(String, CandidateKindKey), Vec<Source>>,
    texts: &mut HashMap<(String, CandidateKindKey), String>,
    kind: CandidateKind,
    text: String,
    source: Source,
) {
    let norm = normalize(&text);
    if norm.is_empty() {
        return;
    }
    let key = match kind {
        CandidateKind::Steering => CandidateKindKey::Steering,
        CandidateKind::SessionMemory => CandidateKindKey::SessionMemory,
    };
    let entry = (norm, key);
    texts.entry(entry.clone()).or_insert(text);
    candidates.entry(entry).or_default().push(source);
}

/// Normalize text for dedupe: lowercase, collapse whitespace.
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// (keyword, topic) pairs for preference mining.
const PREFERENCE_KEYWORDS: &[(&str, &str)] = &[
    ("concise", "concise answers"),
    ("concisely", "concise answers"),
    ("brief", "brief answers"),
    ("detailed", "detailed answers"),
    ("verbose", "detailed answers"),
    ("tabs", "tabs over spaces"),
    ("spaces", "spaces over tabs"),
    ("rust", "rust"),
    ("python", "python"),
    ("dark", "dark mode"),
    ("light", "light mode"),
];

/// Parse a `memory_write layer=.. ns=.. key=..` RunProgress annotation.
fn parse_memory_write(detail: &str) -> Option<(String, String, String)> {
    let rest = detail.strip_prefix("memory_write ")?;
    let mut layer = None;
    let mut ns = None;
    let mut key = None;
    for tok in rest.split_whitespace() {
        let (k, v) = tok.split_once('=')?;
        match k {
            "layer" => layer = Some(v.to_string()),
            "ns" => ns = Some(v.to_string()),
            "key" => key = Some(v.to_string()),
            _ => {}
        }
    }
    Some((layer?, ns?, key?))
}

/// Map the Debug-format layer name from the RunProgress annotation.
fn parse_layer(name: &str) -> Option<LayerKind> {
    match name {
        "Global" => Some(LayerKind::Global),
        "Agent" => Some(LayerKind::Agent),
        "Project" => Some(LayerKind::Project),
        "TaskSession" => Some(LayerKind::TaskSession),
        "EphemeralTurn" => Some(LayerKind::EphemeralTurn),
        _ => None,
    }
}

/// Build a recall query from a memory key: its alphanumeric tokens.
fn key_query(key: &str) -> String {
    key.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

// Small deterministic invariant tests only.
