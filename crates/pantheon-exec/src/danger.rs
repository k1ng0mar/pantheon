//! Dangerous-pattern pre-gate for shell commands.
//!
//! A fast, deterministic, in-process classifier that runs BEFORE any hook,
//! spawn, or policy check. It is deliberately NOT the security boundary:
//! capability policy and (future) sandboxing remain authoritative. This
//! gate exists to fail fast on the unambiguous worst cases (`rm -rf /`,
//! fork bombs) before subprocess latency or plugin involvement, and to
//! give the audit trail a clean `DANGER_BLOCKED` event before any hook
//! noise.
//!
//! Structured output (`DangerAssessment`) leaves room for an AST-aware
//! shell analyzer later without changing call sites.

use pantheon_core::error::{Layer, PantheonError};

/// How dangerous a command is judged to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    /// No known dangerous pattern.
    Low,
    /// Matches a destructive pattern. Blocked outright at the shell tool.
    Critical,
}

/// One pattern hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    pub rule: &'static str,
    pub description: &'static str,
    /// The normalized command text that tripped the rule.
    pub snippet: String,
}

/// Structured result of the classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerAssessment {
    pub level: RiskLevel,
    pub matches: Vec<RuleMatch>,
    /// Command after normalization (quote collapsing, whitespace squeeze).
    /// Kept for future AST-aware analysis and for audit logging.
    pub normalized: String,
}

fn eerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "the command was blocked before execution; if this is intentional, run it yourself in a terminal",
        "",
    )
}

/// Normalization: collapse whitespace runs, strip paired quotes so
/// `rm -rf " / "` variants still match, lowercase for pattern matching.
/// Deliberately not a shell parser; a real parser is the future upgrade
/// path and this stage stays cheap and dependency-free.
fn normalize(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut spaces = 0usize;
    let mut quote: Option<char> = None;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => out.push(c),
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c.is_whitespace() {
                    spaces += 1;
                    if spaces == 1 {
                        out.push(' ');
                    }
                } else {
                    spaces = 0;
                    out.push(c.to_ascii_lowercase());
                }
            }
        }
    }
    out
}

/// Destructive patterns. Matched against the normalized (lowercased,
/// quote-stripped, whitespace-collapsed) command. Substring match is
/// intentional here: the gate is fail-fast, the capability gate and
/// sandbox remain the real boundary, and a false block costs the user
/// a manual terminal run of a command the model should not have run
/// anyway.
/// One arm of the destructive-pattern table: id, human description,
/// predicate over the normalized command.
type Pattern = (&'static str, &'static str, fn(&str) -> bool);

const PATTERNS: &[Pattern] = &[
    (
        "rm_rf_root",
        "recursive force delete of a filesystem root",
        |c: &str| {
            // rm with both r and f flags (any order or bundling) aiming at
            // / or a top-level glob. Segment on ; and && so compound
            // commands still match. Wrapper prefixes (sudo, env, nice,
            // timeout, nohup, command) are stripped before the check.
            fn strip_wrappers(seg: &str) -> &str {
                let mut seg = seg.trim();
                // Iterate: sudo env FOO=1 nice -n 5 rm ... etc.
                loop {
                    let strip = seg
                        .split_whitespace()
                        .next()
                        .map(|first| {
                            matches!(
                                first,
                                "sudo" | "env" | "nice" | "nohup" | "command" | "timeout"
                            ) || first.contains('=')
                        })
                        .unwrap_or(false);
                    if !strip {
                        break;
                    }
                    seg = seg
                        .split_once(char::is_whitespace)
                        .map(|x| x.1)
                        .unwrap_or("")
                        .trim();
                }
                seg
            }
            fn is_root_rm(seg: &str) -> bool {
                let seg = strip_wrappers(seg);
                if !seg.starts_with("rm ") {
                    return false;
                }
                let has_r = seg.split_whitespace().any(|t| {
                    t == "-r" || t == "-rf" || t == "-fr" || (t.starts_with('-') && t.contains('r'))
                });
                let has_f = seg.split_whitespace().any(|t| {
                    t == "-f" || t == "-rf" || t == "-fr" || (t.starts_with('-') && t.contains('f'))
                });
                has_r
                    && has_f
                    && seg
                        .split_whitespace()
                        .skip(1)
                        .filter(|t| !t.starts_with('-'))
                        .any(|t| t == "/" || t == "/*")
            }
            c.split([';', '&']).any(is_root_rm)
        },
    ),
    ("fork_bomb", "shell fork bomb", |c: &str| {
        c.contains(":(){") || c.contains(":(){ :")
    }),
    (
        "disk_wipe",
        "direct write of junk data to a block device",
        |c: &str| {
            c.contains("dd ")
                && (c.contains(" of=/dev/sd")
                    || c.contains(" of=/dev/nvme")
                    || c.contains(" of=/dev/hd")
                    || c.contains(" of=/dev/disk"))
        },
    ),
    (
        "mkfs",
        "filesystem creation on a device (wipes it)",
        |c: &str| c.contains("mkfs") && c.contains("/dev/"),
    ),
    (
        "shred_root",
        "shred of a filesystem root or block device",
        |c: &str| c.starts_with("shred ") && (c.contains(" /dev/") || c.contains(" /boot")),
    ),
    (
        "chmod_root_sweep",
        "chmod 000 on a system root",
        |c: &str| {
            (c.contains("chmod 000 /") || c.contains("chmod -r 000 /")) && !c.contains("/home")
        },
    ),
];

/// Assess a command. Pure function, no I/O, safe to call on every shell
/// request.
pub fn assess(command: &str) -> DangerAssessment {
    let normalized = normalize(command);
    let mut matches = Vec::new();
    for (rule, description, is_match) in PATTERNS {
        if is_match(&normalized) {
            matches.push(RuleMatch {
                rule,
                description,
                snippet: normalized.clone(),
            });
        }
    }
    let level = if matches.is_empty() {
        RiskLevel::Low
    } else {
        RiskLevel::Critical
    };
    DangerAssessment {
        level,
        matches,
        normalized,
    }
}

/// Assess and return a structured refusal for anything Critical. The
/// refusal names every matched rule so the model can correct its plan
/// instead of retrying blindly.
pub fn gate(command: &str) -> Result<(), PantheonError> {
    let a = assess(command);
    if a.level == RiskLevel::Critical {
        let rules: Vec<&str> = a.matches.iter().map(|m| m.rule).collect();
        return Err(eerr(
            "DANGER_BLOCKED",
            format!(
                "command blocked by dangerous-pattern gate: {} (normalized: `{}`)",
                rules.join(", "),
                a.normalized
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "danger_tests.rs"]
mod tests;
