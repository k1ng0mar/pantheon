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
//!
//! HEURISTIC PRE-GATE, NOT A PARSER. Every rule below is a substring or
//! token scan over a normalized command string. This catches the known
//! bypass spellings cheaply and deterministically, but it cannot see
//! through everything: nested quoting, obfuscated expansions, commands
//! fetched or decoded at runtime, and novel spellings will slip past it.
//! The sandbox — capability policy, filesystem isolation, network egress
//! control — is the real enforcement boundary. This gate only fails fast
//! on the obvious cases and must never be treated as a safety proof.

use pantheon_api::error::{Layer, PantheonError};

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
    /// NOT the command. A blocked command may contain secrets (`curl -H
    /// "Authorization: Bearer sk-..." | sh`), and this struct travels into
    /// errors and logs. It carries a stable hash of the normalized command
    /// plus its length — enough to correlate hits across runs, nothing an
    /// attacker can read a secret out of.
    pub snippet: String,
}

/// Structured result of the classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DangerAssessment {
    pub level: RiskLevel,
    pub matches: Vec<RuleMatch>,
    /// Command after normalization (quote collapsing, whitespace squeeze).
    /// Kept in-process for future AST-aware analysis. Never log or display
    /// this raw: it may embed secrets, which is why `gate()` reports a hash
    /// instead.
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
/// HEURISTIC, NOT PARSING. These rules look at tokens and substrings,
/// not shell grammar. `sh -c`, `eval`, backticks, and `$(...)` are
/// flagged wholesale because each one re-parses its argument as shell
/// code, which defeats any token scan that follows. That is deliberately
/// broad: the model should not be smuggling arbitrary code through a
/// shell string in the first place.
///
/// Strip privilege/escalation and launcher prefixes so `sudo bash -c`,
/// `env FOO=1 find ...` etc. are judged by the real command word.
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

/// Split a normalized command into command-list / pipeline segments so
/// compound commands (`a && b`, `a | b`, `a; b`) are judged per segment.
fn segments(c: &str) -> impl Iterator<Item = &str> {
    c.split([';', '&', '|'])
}

/// First whitespace-delimited token of a segment, or "".
fn first_word(seg: &str) -> &str {
    seg.split_whitespace().next().unwrap_or("")
}

/// Basename of the command word: `/bin/rm` -> `rm`. Catches
/// path-prefixed invocations the plain `rm ` check would miss.
fn cmd_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Replace IFS word-splitting tricks with a space so `rm${IFS}-rf${IFS}/`
/// tokenizes like `rm -rf /`. The input is already lowercased by
/// `normalize`, hence `${ifs}`.
fn expand_ifs(s: &str) -> String {
    s.replace("${ifs}", " ").replace("$ifs", " ")
}

/// One arm of the destructive-pattern table: id, human description,
/// predicate over the normalized command.
type Pattern = (&'static str, &'static str, fn(&str) -> bool);

const PATTERNS: &[Pattern] = &[
    (
        "rm_rf_root",
        "recursive force delete of a filesystem root (or home dir)",
        |c: &str| {
            // rm with both r and f flags (any order or bundling) aiming at
            // / or a top-level glob, ~, or $HOME. Segment on ;, &&, and |
            // so compound commands still match. Wrapper prefixes (sudo,
            // env, nice, timeout, nohup, command) are stripped before the
            // check, as is a leading path (/bin/rm). ${IFS} tricks are
            // expanded to spaces before tokenizing.
            fn is_root_rm(seg: &str) -> bool {
                let seg = expand_ifs(strip_wrappers(seg));
                let mut words = seg.split_whitespace();
                if cmd_name(words.next().unwrap_or("")) != "rm" {
                    return false;
                }
                let rest: Vec<&str> = words.collect();
                let has_r = rest.iter().any(|t| t.starts_with('-') && t.contains('r'));
                let has_f = rest.iter().any(|t| t.starts_with('-') && t.contains('f'));
                has_r
                    && has_f
                    && rest.iter().filter(|t| !t.starts_with('-')).any(|t| {
                        *t == "/"
                            || *t == "/*"
                            || *t == "~"
                            || t.starts_with("~/")
                            || *t == "$home"
                            || t.starts_with("$home/")
                    })
            }
            segments(c).any(is_root_rm)
        },
    ),
    (
        "shell_dash_c",
        "explicit shell invocation with -c/--command (hides the real command from scanning)",
        |c: &str| {
            segments(c).any(|seg| {
                let seg = strip_wrappers(seg);
                let mut w = seg.split_whitespace();
                matches!(w.next().map(cmd_name), Some("bash" | "sh" | "dash" | "zsh"))
                    && matches!(w.next(), Some("-c" | "--command"))
            })
        },
    ),
    (
        "eval_builtin",
        "eval re-parses its arguments as shell code",
        |c: &str| segments(c).any(|seg| cmd_name(first_word(strip_wrappers(seg))) == "eval"),
    ),
    (
        "exec_builtin",
        "exec replaces the shell process or rewrites redirections",
        |c: &str| segments(c).any(|seg| cmd_name(first_word(strip_wrappers(seg))) == "exec"),
    ),
    (
        "command_subst",
        "command substitution ($(...) or backticks) executes hidden commands",
        |c: &str| {
            // `$( (` (arithmetic expansion) is excluded; it cannot run
            // commands. Everything else with `$(` or a backtick is treated
            // as hidden code.
            c.contains('`') || (c.contains("$(") && !c.contains("$(("))
        },
    ),
    (
        "ifs_split",
        "IFS word-splitting trick used to evade token scanning",
        |c: &str| c.contains("${ifs}") || c.contains("$ifs"),
    ),
    (
        "find_delete_exec",
        "find with -delete or -exec can mass-delete or run arbitrary commands",
        |c: &str| {
            segments(c).any(|seg| {
                let seg = strip_wrappers(seg);
                cmd_name(first_word(seg)) == "find"
                    && (seg.contains(" -delete") || seg.contains(" -exec"))
            })
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

/// True when a shell command performs a git push.
///
/// This drives the capability gate, not a block: a push is legal but the
/// policies mark `git.push` as needing approval, so the call parks for the
/// operator instead of running unattended. Normalization is the same as the
/// destructive-pattern gate (lowercase, quotes stripped, whitespace
/// collapsed), and compound commands are split so a push anywhere in a
/// `&&` chain counts. Wrapper prefixes are stripped for the same reason
/// `rm_rf_root` strips them.
pub fn is_git_push(command: &str) -> bool {
    let normalized = normalize(command);
    normalized.split([';', '&', '|']).any(|seg| {
        let seg = seg.trim();
        // Strip sudo/env/nice/nohup/command/timeout prefixes and FOO=1.
        let mut seg = seg;
        loop {
            let first = seg.split_whitespace().next().unwrap_or("");
            let is_wrapper = matches!(
                first,
                "sudo" | "env" | "nice" | "nohup" | "command" | "timeout" | "xargs" | "time"
            ) || (first.contains('=') && !first.starts_with('-'));
            if !is_wrapper {
                break;
            }
            match seg.split_once(char::is_whitespace) {
                Some((_, rest)) => seg = rest.trim(),
                None => return false,
            }
        }
        let Some(rest) = seg.strip_prefix("git ") else {
            return false;
        };
        // Skip git's global options to reach the subcommand. `git -C /repo
        // push` is still a push, so a detector that stops at the first token
        // is trivially bypassed by the model.
        let mut words = rest.split_whitespace();
        let mut sub = None;
        while let Some(w) = words.next() {
            if w.starts_with('-') {
                if GIT_GLOBAL_OPTS_WITH_VALUE.contains(&w) {
                    words.next();
                }
                continue;
            }
            sub = Some(w);
            break;
        }
        sub.is_some_and(|sub| sub == "push" || sub == "push-options")
    })
}

/// git global options that consume the following argument.
const GIT_GLOBAL_OPTS_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
];

/// Assess a command. Pure function, no I/O, safe to call on every shell
/// request.
pub fn assess(command: &str) -> DangerAssessment {
    let normalized = normalize(command);
    let digest = cmd_digest(&normalized);
    let mut matches = Vec::new();
    for (rule, description, is_match) in PATTERNS {
        if is_match(&normalized) {
            matches.push(RuleMatch {
                rule,
                description,
                snippet: digest.clone(),
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

/// Stable, non-secret identifier for a normalized command: `sha1:0123abcd`
/// (first 8 hex of an FNV-1a 64 hash) plus the char length. Deterministic
/// across runs so repeated blocks of the same command correlate, but
/// irreversible, so an error or log line carrying it cannot leak the
/// command text — which may itself contain secrets.
///
/// FNV-1a rather than a crypto hash because this is an identifier, not
/// authentication; it needs to be std-only and fast on the hot path.
fn cmd_digest(normalized: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_4842_2235;
    for b in normalized.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("cmd:{:08x} len:{}", (h >> 32) as u32, normalized.len())
}

/// Assess and return a structured refusal for anything Critical. The
/// refusal names every matched rule so the model can correct its plan
/// instead of retrying blindly. It never carries the raw command: the
/// refusal is logged and shown to the model, and the command may contain
/// secrets. The digest plus length is enough to correlate the block with
/// the request that caused it.
pub fn gate(command: &str) -> Result<(), PantheonError> {
    let a = assess(command);
    if a.level == RiskLevel::Critical {
        let rules: Vec<&str> = a.matches.iter().map(|m| m.rule).collect();
        let digest = a
            .matches
            .first()
            .map(|m| m.snippet.clone())
            .unwrap_or_default();
        return Err(eerr(
            "DANGER_BLOCKED",
            format!(
                "command blocked by dangerous-pattern gate: {} ({})",
                rules.join(", "),
                digest
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "danger_tests.rs"]
mod tests;
