//! Failure recurrence ledger: the same failure class, counted.
//!
//! A gate card tells the operator about one turn. This ledger answers the
//! question the card cannot: *has this happened before, and did it come back
//! after we called it fixed?* Every occurrence gets a stable class id derived
//! from what failed (not from the wording), so a repeated failure groups.
//!
//! Storage is one append-only JSONL file in the data dir, `recurrence.jsonl`.
//! Two event kinds share it:
//!   {"kind":"hit","ts":...,"class":"verify:no-test-probe",...}
//!   {"kind":"fixed","ts":...,"class":"..."}
//! A hit after the newest `fixed` event reopens the class. No schema
//! migration, no database, and deleting the file is safe.

use super::{BlockKind, TranscriptBlock};
use std::io::Write;
use std::path::Path;

pub const FILE: &str = "recurrence.jsonl";
/// past this size the file keeps its newest slice instead of growing forever
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// lines kept when the file is trimmed
const KEEP_LINES: usize = 1000;
/// occurrence lines read for display, newest last
const READ_LINES: usize = 2000;

/// one line in the ledger
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub ts_ms: i64,
    /// stable class id, e.g. `verify:no-test-probe`
    pub class: String,
    /// human label for the class family, e.g. "verification gate"
    pub label: String,
    /// what happened this time (sentence / error text, clipped)
    pub detail: String,
    pub run_id: String,
    /// `true` for a "fixed" marker; those have no detail
    pub fixed: bool,
}

/// aggregated view of one class
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassRow {
    pub class: String,
    pub label: String,
    pub hits: usize,
    pub first_ms: i64,
    pub last_ms: i64,
    /// timestamp of the newest `fixed` marker, if any
    pub fixed_ms: Option<i64>,
    /// hits after the newest fixed marker: it came back
    pub reopened: bool,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// stable class for a verification-gate finding. Fixed strings, not the
/// finding's sentence, so two wordings of the same miss group together.
pub fn verify_class(reason: &str) -> String {
    let class = if reason.contains("no test command") {
        "no-test-probe"
    } else if reason.contains("no build or compile") {
        "no-build-probe"
    } else if reason.contains("no deploy or publish") {
        "no-deploy-probe"
    } else if reason.contains("should work") {
        "should-work"
    } else if reason.contains("files changed but no command") {
        "unchecked-edit"
    } else {
        return format!("verify:{}", slug(reason));
    };
    format!("verify:{class}")
}

pub fn tool_class(name: &str) -> String {
    format!("tool:{}", slug(name))
}

pub fn provider_class(kind: &str) -> String {
    format!("provider:{}", slug(kind))
}

/// occurrences the finished turn produced: failed tool calls, provider
/// failures, and gate findings that are already in the transcript.
pub fn turn_occurrences(run_id: &str, blocks: &[TranscriptBlock]) -> Vec<Occurrence> {
    let ts = now_ms();
    let start = blocks
        .iter()
        .rposition(|b| matches!(b.kind, BlockKind::UserMessage(_)))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut out = Vec::new();
    for b in &blocks[start..] {
        match &b.kind {
            BlockKind::ToolCall {
                name, ok, error, ..
            } if *ok == Some(false) || error.as_deref().is_some_and(|e| !e.trim().is_empty()) => {
                let detail = error.clone().unwrap_or_else(|| "tool call failed".into());
                out.push(Occurrence {
                    ts_ms: ts,
                    class: tool_class(name),
                    label: format!("tool failure · {name}"),
                    detail: clip(&detail, 200),
                    run_id: run_id.to_string(),
                    fixed: false,
                });
            }
            BlockKind::ProviderError {
                provider,
                model,
                kind,
                ..
            } => out.push(Occurrence {
                ts_ms: ts,
                class: provider_class(&format!("{kind:?}")),
                label: format!("provider error · {provider}/{model}"),
                detail: format!("{kind:?}"),
                run_id: run_id.to_string(),
                fixed: false,
            }),
            BlockKind::VerifyGate(findings) => {
                for f in findings {
                    out.push(Occurrence {
                        ts_ms: ts,
                        class: verify_class(&f.reason),
                        label: "verification gate".into(),
                        detail: format!("{} ({})", f.sentence, f.reason),
                        run_id: run_id.to_string(),
                        fixed: false,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        return one;
    }
    let mut out: String = one.chars().take(n.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

fn hit_json(o: &Occurrence) -> String {
    serde_json::json!({
        "kind": "hit",
        "ts": o.ts_ms,
        "class": o.class,
        "label": o.label,
        "detail": o.detail,
        "run": o.run_id,
    })
    .to_string()
}

fn fixed_json(class: &str, ts: i64) -> String {
    serde_json::json!({"kind": "fixed", "ts": ts, "class": class}).to_string()
}

/// Append occurrences, trimming the file first when it grew too large.
/// Failures are silent by contract: bookkeeping must never break a turn.
pub fn append(path: &Path, occurrences: &[Occurrence]) {
    if occurrences.is_empty() {
        return;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_FILE_BYTES {
            trim(path);
        }
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let mut buf = String::new();
    for o in occurrences {
        buf.push_str(&hit_json(o));
        buf.push('\n');
    }
    let _ = f.write_all(buf.as_bytes());
}

fn trim(path: &Path) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(KEEP_LINES).collect();
    let kept = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    let _ = std::fs::write(path, format!("{kept}\n"));
}

/// Mark a class fixed. Returns false when the class has never been recorded,
/// so the operator gets an honest answer instead of a write that hides a typo.
pub fn mark_fixed(path: &Path, class: &str) -> bool {
    mark_fixed_at(path, class, now_ms())
}

/// `mark_fixed` with an explicit timestamp, for tests that pin ordering.
fn mark_fixed_at(path: &Path, class: &str, ts: i64) -> bool {
    if !load(path).iter().any(|o| o.class == class) {
        return false;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return false;
    };
    let _ = writeln!(f, "{}", fixed_json(class, ts));
    true
}

/// Read the ledger, newest last. Malformed lines are skipped, never fatal.
pub fn load(path: &Path) -> Vec<Occurrence> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let start = text.lines().count().saturating_sub(READ_LINES);
    text.lines()
        .skip(start)
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let class = v.get("class")?.as_str()?.to_string();
            let ts_ms = v.get("ts")?.as_i64()?;
            let kind = v.get("kind")?.as_str().unwrap_or("hit");
            Some(Occurrence {
                ts_ms,
                class,
                label: v
                    .get("label")
                    .and_then(|l| l.as_str())
                    .unwrap_or("")
                    .to_string(),
                detail: v
                    .get("detail")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                run_id: v
                    .get("run")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string(),
                fixed: kind == "fixed",
            })
        })
        .collect()
}

/// Group by class, newest activity first. A hit newer than the newest `fixed`
/// marker reopens the class; `fixed` markers with no later hit stay closed.
pub fn aggregate(occurrences: &[Occurrence]) -> Vec<ClassRow> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, ClassRow> = BTreeMap::new();
    for o in occurrences {
        let row = map.entry(o.class.clone()).or_insert_with(|| ClassRow {
            class: o.class.clone(),
            label: o.label.clone(),
            hits: 0,
            first_ms: o.ts_ms,
            last_ms: o.ts_ms,
            fixed_ms: None,
            reopened: false,
        });
        if !row.label.is_empty() {
            row.label = o.label.clone();
        }
        if o.fixed {
            row.fixed_ms = Some(o.ts_ms);
        } else {
            row.hits += 1;
            row.first_ms = row.first_ms.min(o.ts_ms);
            row.last_ms = row.last_ms.max(o.ts_ms);
        }
    }
    let mut rows: Vec<ClassRow> = map.into_values().collect();
    for r in rows.iter_mut() {
        r.reopened = matches!(r.fixed_ms, Some(f) if r.last_ms > f);
    }
    rows.sort_by_key(|a| std::cmp::Reverse(a.last_ms));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-recurrence-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE)
    }

    fn occ(class: &str, ts: i64) -> Occurrence {
        Occurrence {
            ts_ms: ts,
            class: class.into(),
            label: "verification gate".into(),
            detail: "tests pass (no test command ran)".into(),
            run_id: "r1".into(),
            fixed: false,
        }
    }

    #[test]
    fn verify_reasons_map_to_stable_classes_not_wording() {
        assert_eq!(verify_class("no test command ran in this turn"), "verify:no-test-probe");
        assert_eq!(verify_class("no build or compile command ran in this turn"), "verify:no-build-probe");
        assert_eq!(verify_class("\"should work\" is a guess, not evidence"), "verify:should-work");
        assert_eq!(tool_class("shell"), "tool:shell");
        assert_eq!(provider_class("RateLimited"), "provider:ratelimited");
    }

    #[test]
    fn append_then_load_round_trips_and_skips_junk() {
        let path = scratch("roundtrip");
        append(&path, &[occ("verify:no-test-probe", 10), occ("tool:shell", 20)]);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].class, "verify:no-test-probe");
        assert_eq!(loaded[1].class, "tool:shell");
        assert!(!loaded[0].fixed);
    }

    #[test]
    fn aggregate_counts_first_last_and_flags_a_reopen() {
        let path = scratch("aggregate");
        append(
            &path,
            &[
                occ("verify:no-test-probe", 100),
                occ("verify:no-test-probe", 200),
                occ("tool:shell", 150),
            ],
        );
        assert!(mark_fixed_at(&path, "verify:no-test-probe", 250));
        append(&path, &[occ("verify:no-test-probe", 300)]);
        let rows = aggregate(&load(&path));
        assert_eq!(rows.len(), 2);
        let v = rows.iter().find(|r| r.class == "verify:no-test-probe").unwrap();
        assert_eq!(v.hits, 3);
        assert_eq!(v.first_ms, 100);
        assert_eq!(v.last_ms, 300);
        assert_eq!(v.fixed_ms, Some(250));
        assert!(v.reopened, "a hit after the fix reopens the class");
        let t = rows.iter().find(|r| r.class == "tool:shell").unwrap();
        assert!(!t.reopened);
        assert_eq!(rows[0].class, "verify:no-test-probe", "newest activity first");
    }

    #[test]
    fn marking_an_unknown_class_fixed_changes_nothing() {
        let path = scratch("unknown");
        append(&path, &[occ("tool:shell", 1)]);
        assert!(!mark_fixed(&path, "verify:made-up"));
        assert_eq!(load(&path).len(), 1, "no marker was written");
    }

    #[test]
    fn turn_occurrences_read_failed_tools_gate_and_provider_cards() {
        let blocks = vec![
            TranscriptBlock {
                kind: BlockKind::UserMessage("do it".into()),
            },
            TranscriptBlock {
                kind: BlockKind::ToolCall {
                    name: "shell".into(),
                    args: "cargo test".into(),
                    ok: Some(false),
                    call_id: "c1".into(),
                    started: None,
                    duration: None,
                    error: Some("exit 101".into()),
                    tokens: None,
                    expanded: false,
                    output: Vec::new(),
                },
            },
            TranscriptBlock {
                kind: BlockKind::VerifyGate(vec![super::super::verify_gate::Finding {
                    sentence: "Tests pass.".into(),
                    reason: "no test command ran in this turn".into(),
                }]),
            },
        ];
        let out = turn_occurrences("run-1", &blocks);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|o| o.class == "tool:shell"));
        assert!(out.iter().any(|o| o.class == "verify:no-test-probe"));
        assert!(out.iter().all(|o| o.run_id == "run-1"));
    }

    #[test]
    fn trimmed_file_keeps_the_newest_lines_only() {
        let path = scratch("trim");
        let old = (0..KEEP_LINES + 50)
            .map(|i| hit_json(&occ("tool:shell", i as i64)))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{old}\n")).unwrap();
        trim(&path);
        let loaded = load(&path);
        assert_eq!(loaded.len(), KEEP_LINES);
        assert_eq!(loaded.last().unwrap().ts_ms, (KEEP_LINES + 49) as i64);
    }
}
