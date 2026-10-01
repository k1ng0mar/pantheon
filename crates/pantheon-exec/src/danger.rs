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
/// `is_git_push` is the exception: it is a real shell-word parser
/// (see its docs), not a substring scan.
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

/// ADVISORY DETECTION, NOT A SECURITY BOUNDARY.
///
/// `is_git_push` is an early-warning heuristic, not a guarantee. It is a
/// shell-word parser that spots git pushes written out in the open, so the
/// model cannot run `git push` unnoticed through the `shell` tool. A purely
/// syntactic parser cannot be complete here: opaque shell constructions
/// can hide a push from it by construction — interpreters (`sh -c`,
/// `bash -c`, `python -c`, `perl -e`, ...), `find -exec`, scripts arriving
/// on stdin, function shadowing, git `!`-aliases, launcher wrappers that
/// forward argv, field-splitting tricks, and anything decoded or fetched
/// at runtime. A clean scan is therefore NOT proof there is no push; it
/// only means no push was seen.
///
/// INTENDED FUTURE ARCHITECTURE: authoritative enforcement belongs at the
/// exec/policy boundary, not in this parser. The plan is a final-argv check
/// — a `git` argv whose subcommand is `push` requires approval — with
/// every opaque shell construction (interpreters, `eval`, `sh -c`-style
/// re-parse, unreadable argv) classified as opaque execution under the
/// policy, so hiding a push behind opacity is itself the gated event.
/// Until that lands, this heuristic is the best the capability gate has:
/// useful, advisory, and fallible by design.
///
/// True when a shell command performs a git push.
///
/// This drives the capability gate, not a block: a push is legal but the
/// policies mark `git.push` as needing approval, so the call parks for the
/// operator instead of running unattended.
///
/// This is a REAL shell-word parser, not a substring scan. Detection
/// semantics (matching is case-insensitive; the conservative direction
/// applies throughout — when a word's static value cannot be determined,
/// it counts as a push and the operator decides):
///
/// - The command is tokenized quote-aware: single/double quotes, backslash
///   escapes (including line continuations), `$'...'` ANSI-C quoting,
///   `$(...)`, backticks, `${...}`, and `<(...)` / `>(...)` process
///   substitution never split a word; `;`, `&`, `&&`, `||`, `|`, newlines,
///   `(`, `)`, `{`, `}`, `!`, and redirections always do. `$IFS`/`${IFS}`
///   outside single quotes are word separators, as in the shell. Heredoc
///   bodies are skipped by the tokenizer so heredoc *text* is never
///   mistaken for commands.
/// - Compound commands are analyzed structurally: subshells `( )`, brace
///   groups `{ }`, `!` negation, `if`/`then`/`elif`/`else`/`fi`,
///   `while`/`until`/`do`/`done`, `for`/`select`/`do`/`done`,
///   `case`/`esac`, `[[ ]]`, and function definitions (calling a function
///   whose body pushes counts).
/// - Each simple command's words are statically expanded: quote removal,
///   backslash removal, `$'...'` escapes, `${v:-default}` defaults,
///   `$v`/`${v}` from leading `name=value` assignments in the same command
///   string, brace expansion (`p{u,}sh`), and statically resolvable
///   `$(...)`/backticks (`echo`/`printf`/`which`/`command -v` shapes). A
///   word with an unresolvable expansion is opaque.
/// - Wrapper peeling consumes wrapper ARGUMENTS, not just wrapper words:
///   `sudo` (`-u`/`-g`/`-p`/..., `VAR=x`), `env` (`-i`, `-u`, `VAR=x`),
///   `timeout` (flags + duration), `command` (`-p`; `-v`/`-V` never
///   execute), `nice` (`-n N`), `stdbuf` (`-o`/`-e`/`-i`), `setsid`,
///   `nohup`, `time`, `exec`, `builtin`, `coproc`, and `xargs` (option
///   shapes plus the unknown-stdin-args rule). Layers nest arbitrarily.
/// - A simple command counts when its command word (basename, after
///   expansion) is `git` and the first non-option word after git's global
///   options is `push` or `push-options` — or is opaque. An opaque command
///   word with a `push`/opaque argument counts too.
/// - `eval <code>` scans `<code>` as shell; `sh`/`bash`/`dash`/`zsh`
///   `-c`/`--command <code>` scans `<code>` as shell (opaque code counts).
/// - Every `$(...)`/backtick/`<(`/`>(` body is scanned recursively as its
///   own command string (quote-aware: single quotes suppress substitution,
///   double quotes do not).
///
/// Documented non-goals: expansions the parser cannot see through
/// (`${x:0:3}` slicing, `${v//pat/rep}` rewriting, `$?`/`$1`/etc. — all
/// opaque, hence flagged when they sit in command/subcommand position),
/// code fetched or decoded at runtime, and substitution-shaped text inside
/// quoted heredoc bodies (flagged conservatively).
pub fn is_git_push(command: &str) -> bool {
    // `$IFS` / `${IFS}` are word separators in the shell; rewrite them to
    // spaces outside single quotes before tokenizing.
    let desplit = expand_ifs_words(command);
    analyze_script(&desplit, &mut PushCtx::new(), 0)
}

// ---------------------------------------------------------------------------
// Shell-word parser backing `is_git_push`.
// ---------------------------------------------------------------------------

/// Static value of one shell word after quote/escape/expansion processing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WordVal {
    /// Fully expanded alternatives. Brace expansion can yield several
    /// (`p{u,}sh` -> `push`, `psh`); every other word yields exactly one.
    Known(Vec<String>),
    /// Contains an unresolvable expansion (`$(mystery)`, `$undef`,
    /// `${x:+...}`, `$?`, ...): the runtime value is unknowable statically.
    Opaque,
}

/// Per-command-string analysis context: tracked variables and function
/// definitions seen so far.
#[derive(Debug, Default)]
struct PushCtx {
    /// `name=value` assignments; `None` value = assigned but opaque.
    vars: Vec<(String, Option<String>)>,
    /// Lowercased names of functions whose body performs a push.
    push_fns: Vec<String>,
}

impl PushCtx {
    fn new() -> Self {
        PushCtx::default()
    }
}

/// Tokens of a shell command line. Words keep their raw text (quotes and
/// escapes intact); expansion happens later, per word.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    Semi,   // ;
    Amp,    // &
    AndAnd, // &&
    Pipe,   // |
    OrOr,   // ||
    Newline,
    SubOpen(bool), // ( ; bool = immediately followed by `(`
    SubClose,      // )
    BraceOpen,     // {
    BraceClose,    // }
    Bang,          // !
    Redir,         // < > << >> <<< <> >& <& >| &> and fd-prefixed forms
}

/// Tokenize a command line, respecting quotes, escapes, `$` constructs,
/// and process substitution. This is the structural foundation the old
/// blocklist approach lacked: a metachar inside quotes never splits a
/// word, and a word split never happens inside `$(...)`.
fn tokenize(s: &str) -> Vec<Tok> {
    let c: Vec<char> = s.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    let flush = |buf: &mut String, toks: &mut Vec<Tok>| {
        if !buf.is_empty() {
            toks.push(Tok::Word(std::mem::take(buf)));
        }
    };
    while i < c.len() {
        let ch = c[i];
        match ch {
            '\'' | '"' | '`' => {
                // Quoted/backquoted sections never split a word.
                let (text, next) = consume_quoted(&c, i);
                buf.push_str(&text);
                i = next;
            }
            '\\' => {
                buf.push('\\');
                if i + 1 < c.len() {
                    buf.push(c[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '$' => {
                let (text, next) = consume_dollar(&c, i);
                buf.push_str(&text);
                i = next;
            }
            ' ' | '\t' | '\r' => {
                flush(&mut buf, &mut toks);
                i += 1;
            }
            '\n' => {
                flush(&mut buf, &mut toks);
                toks.push(Tok::Newline);
                i += 1;
            }
            '#' if buf.is_empty() => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            ';' => {
                flush(&mut buf, &mut toks);
                if i + 1 < c.len() && c[i + 1] == ';' {
                    toks.push(Tok::Semi);
                    toks.push(Tok::Semi);
                    i += 2;
                } else {
                    toks.push(Tok::Semi);
                    i += 1;
                }
            }
            '&' => {
                flush(&mut buf, &mut toks);
                if i + 1 < c.len() && c[i + 1] == '&' {
                    toks.push(Tok::AndAnd);
                    i += 2;
                } else if i + 1 < c.len() && c[i + 1] == '>' {
                    toks.push(Tok::Redir);
                    i += if i + 2 < c.len() && c[i + 2] == '>' {
                        3
                    } else {
                        2
                    };
                } else {
                    toks.push(Tok::Amp);
                    i += 1;
                }
            }
            '|' => {
                flush(&mut buf, &mut toks);
                if i + 1 < c.len() && c[i + 1] == '|' {
                    toks.push(Tok::OrOr);
                    i += 2;
                } else {
                    toks.push(Tok::Pipe);
                    i += 1;
                }
            }
            '(' => {
                flush(&mut buf, &mut toks);
                let glued = i + 1 < c.len() && c[i + 1] == '(';
                toks.push(Tok::SubOpen(glued));
                i += 1;
            }
            ')' => {
                flush(&mut buf, &mut toks);
                toks.push(Tok::SubClose);
                i += 1;
            }
            '{' => {
                // `{` opens a brace group only when it stands alone;
                // otherwise it is brace expansion or literal text.
                if buf.is_empty() && (i + 1 >= c.len() || c[i + 1].is_whitespace()) {
                    toks.push(Tok::BraceOpen);
                    i += 1;
                } else {
                    buf.push(ch);
                    i += 1;
                }
            }
            '}' => {
                if buf.is_empty() {
                    toks.push(Tok::BraceClose);
                    i += 1;
                } else {
                    buf.push(ch);
                    i += 1;
                }
            }
            '!' => {
                let alone = buf.is_empty()
                    && (i + 1 >= c.len()
                        || c[i + 1].is_whitespace()
                        || matches!(c[i + 1], ';' | '&' | '|' | '(' | ')' | '<' | '>'));
                if alone {
                    toks.push(Tok::Bang);
                    i += 1;
                } else {
                    buf.push(ch);
                    i += 1;
                }
            }
            '<' | '>' => {
                if !buf.is_empty() && !buf.chars().all(|d| d.is_ascii_digit()) {
                    flush(&mut buf, &mut toks);
                } else {
                    buf.clear(); // fd prefix (`2>`) merges into the redirect
                }
                i = consume_redirect(&c, i, &mut toks, &mut buf);
            }
            _ => {
                buf.push(ch);
                i += 1;
            }
        }
    }
    flush(&mut buf, &mut toks);
    toks
}

/// Consume a quoted (`'...'`, `"..."`) or backquoted (`` `...` ``) section
/// starting at `c[i]`. A backslash escapes the closer for `"` and `` ` ``;
/// inside single quotes nothing is special.
fn consume_quoted(c: &[char], i: usize) -> (String, usize) {
    let q = c[i];
    let mut j = i + 1;
    while j < c.len() {
        if q != '\'' && c[j] == '\\' {
            j += 2;
            continue;
        }
        if c[j] == q {
            break;
        }
        j += 1;
    }
    let end = if j < c.len() { j + 1 } else { j };
    (c[i..end].iter().collect(), end)
}

/// Consume a `$...` construct starting at `c[i] == '$'` into the word
/// buffer: `$(...)` (balanced, quote-aware), `${...}` (balanced),
/// `$'...'` / `$"..."`, or `$name` / `$1` / `$?` / ...
fn consume_dollar(c: &[char], i: usize) -> (String, usize) {
    if i + 1 >= c.len() {
        return ("$".to_string(), i + 1);
    }
    match c[i + 1] {
        '(' => {
            if i + 2 < c.len() && c[i + 2] == '(' {
                match match_paren_chars(c, i + 1) {
                    Some(end) => (c[i..end].iter().collect(), end),
                    None => ("$".to_string(), i + 1),
                }
            } else {
                match close_paren(c, i + 2) {
                    Some(j) => (c[i..=j].iter().collect(), j + 1),
                    None => ("$".to_string(), i + 1),
                }
            }
        }
        '{' => match close_brace(c, i + 2) {
            Some(j) => (c[i..=j].iter().collect(), j + 1),
            None => ("$".to_string(), i + 1),
        },
        '\'' | '"' => {
            let (text, end) = consume_quoted(c, i + 1);
            let mut s = String::from("$");
            s.push_str(&text);
            (s, end)
        }
        _ => {
            let mut j = i + 1;
            if c[j].is_ascii_alphabetic() || c[j] == '_' {
                j += 1;
                while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '_') {
                    j += 1;
                }
            } else {
                j += 1; // `$?`, `$$`, `$1`, ...: one char
            }
            (c[i..j].iter().collect(), j)
        }
    }
}

/// Exclusive end index of the paren group opened at `open` (which points
/// at the `(` itself), or `None` if unbalanced. Quote-aware.
fn match_paren_chars(c: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut q: Option<char> = None;
    let mut j = open;
    while j < c.len() {
        let ch = c[j];
        if let Some(qq) = q {
            if ch == qq {
                q = None;
            } else if ch == '\\' && qq == '"' {
                j += 1;
            }
        } else {
            match ch {
                '"' | '\'' => q = Some(ch),
                '\\' => j += 1,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j + 1);
                    }
                }
                _ => {}
            }
        }
        j += 1;
    }
    None
}

/// Consume a `<` / `>` redirect starting at `c[i]`. Heredocs (`<<[-]DELIM`)
/// are skipped through the body here so heredoc *text* is never mistaken
/// for commands; process substitution (`<(...)` / `>(...)`) is kept inside
/// the current word. Returns the next index.
fn consume_redirect(c: &[char], i: usize, toks: &mut Vec<Tok>, buf: &mut String) -> usize {
    let ch = c[i];
    // Herestring `<<<`.
    if ch == '<' && i + 2 < c.len() && c[i + 1] == '<' && c[i + 2] == '<' {
        toks.push(Tok::Redir);
        return i + 3;
    }
    // Heredoc `<<[-]DELIM`.
    if ch == '<' && i + 1 < c.len() && c[i + 1] == '<' {
        let mut j = i + 2;
        let strip_tabs = j < c.len() && c[j] == '-';
        if strip_tabs {
            j += 1;
        }
        while j < c.len() && (c[j] == ' ' || c[j] == '\t') {
            j += 1;
        }
        let mut delim = String::new();
        if j < c.len() && (c[j] == '\'' || c[j] == '"') {
            let q = c[j];
            j += 1;
            while j < c.len() && c[j] != q {
                delim.push(c[j]);
                j += 1;
            }
            j += 1;
        } else {
            while j < c.len()
                && !c[j].is_whitespace()
                && !matches!(c[j], ';' | '&' | '|' | '<' | '>')
            {
                if c[j] == '\\' && j + 1 < c.len() {
                    delim.push(c[j + 1]);
                    j += 2;
                } else {
                    delim.push(c[j]);
                    j += 1;
                }
            }
        }
        while j < c.len() && c[j] != '\n' {
            j += 1;
        }
        loop {
            if j < c.len() && c[j] == '\n' {
                j += 1;
            }
            let start = j;
            while j < c.len() && c[j] != '\n' {
                j += 1;
            }
            let mut line: String = c[start..j].iter().collect();
            if strip_tabs {
                line = line.trim_start_matches('\t').to_string();
            }
            if line == delim || j >= c.len() {
                break;
            }
        }
        toks.push(Tok::Redir);
        return j;
    }
    // Process substitution `<(...)` / `>(...)` stays inside the word.
    if i + 1 < c.len() && c[i + 1] == '(' {
        match match_paren_chars(c, i + 1) {
            Some(end) => {
                buf.push_str(&c[i..end].iter().collect::<String>());
                return end;
            }
            None => {
                buf.push(ch);
                return i + 1;
            }
        }
    }
    // Longest match on the remaining operator chars.
    let mut j = i + 1;
    if j < c.len() {
        if c[j] == ch {
            j += 1; // << >>
        } else if ch == '<' && (c[j] == '>' || c[j] == '&') {
            j += 1; // <> <&
        } else if ch == '>' && (c[j] == '&' || c[j] == '|') {
            j += 1; // >& >|
        }
    }
    toks.push(Tok::Redir);
    j
}

/// Expand one raw word to its static value.
fn expand_word(raw: &str, ctx: &PushCtx) -> WordVal {
    let c: Vec<char> = raw.chars().collect();
    match expand_seq(&c, ctx) {
        Some(alts) => WordVal::Known(alts),
        None => WordVal::Opaque,
    }
}

/// Expand a char slice, multiplying brace-expansion alternatives.
/// `None` = some part is statically unknowable.
fn expand_seq(c: &[char], ctx: &PushCtx) -> Option<Vec<String>> {
    match find_brace_group(c) {
        None => expand_simple(c, ctx).map(|s| vec![s]),
        Some((open, close, alts)) => {
            let mut out = Vec::new();
            for alt in alts {
                let mut recombined = Vec::with_capacity(c.len());
                recombined.extend_from_slice(&c[..open]);
                recombined.extend_from_slice(&alt);
                recombined.extend_from_slice(&c[close + 1..]);
                out.extend(expand_seq(&recombined, ctx)?);
            }
            Some(out)
        }
    }
}

/// Find the first unquoted `{...}` group containing a top-level comma.
/// Returns the brace indexes and the alternative char slices.
fn find_brace_group(c: &[char]) -> Option<(usize, usize, Vec<Vec<char>>)> {
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' => i += 2,
            '\'' => {
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            '$' => {
                let (_, next) = consume_dollar(c, i);
                i = next.max(i + 1);
            }
            '{' => {
                let mut depth = 1usize;
                let mut q: Option<char> = None;
                let mut commas = Vec::new();
                let mut j = i + 1;
                while j < c.len() {
                    let ch = c[j];
                    if let Some(qq) = q {
                        if ch == qq {
                            q = None;
                        }
                    } else if ch == '"' || ch == '\'' {
                        q = Some(ch);
                    } else if ch == '\\' {
                        j += 1;
                    } else if ch == '{' {
                        depth += 1;
                    } else if ch == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    } else if ch == ',' && depth == 1 {
                        commas.push(j);
                    }
                    j += 1;
                }
                if j < c.len() && !commas.is_empty() {
                    let mut alts = Vec::new();
                    let mut start = i + 1;
                    for cp in commas {
                        alts.push(c[start..cp].to_vec());
                        start = cp + 1;
                    }
                    alts.push(c[start..j].to_vec());
                    return Some((i, j, alts));
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Expand a word slice with no brace-group multiplication: quotes,
/// backslashes, `$` constructs, and backticks are processed.
/// `None` = opaque.
fn expand_simple(c: &[char], ctx: &PushCtx) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\'' => {
                let mut j = i + 1;
                while j < c.len() && c[j] != '\'' {
                    j += 1;
                }
                out.push_str(&c[i + 1..j.min(c.len())].iter().collect::<String>());
                i = if j < c.len() { j + 1 } else { j };
            }
            '"' => {
                let (inner, next) = consume_dquoted(c, i);
                out.push_str(&expand_dquoted(&inner, ctx)?);
                i = next;
            }
            '\\' => {
                if i + 1 < c.len() {
                    if c[i + 1] == '\n' {
                        i += 2; // line continuation: vanishes
                    } else {
                        out.push(c[i + 1]);
                        i += 2;
                    }
                } else {
                    i += 1;
                }
            }
            '$' => {
                let (s, next) = expand_dollar(c, i, ctx)?;
                out.push_str(&s);
                i = next;
            }
            '`' => match find_closing_backtick(c, i + 1) {
                Some(j) => {
                    let body: String = c[i + 1..j].iter().collect();
                    out.push_str(&resolve_subst_output(&body)?);
                    i = j + 1;
                }
                None => {
                    out.push('`');
                    i += 1;
                }
            },
            _ => {
                out.push(c[i]);
                i += 1;
            }
        }
    }
    Some(out)
}

/// Consume a `"..."` section starting at `c[i]`, honoring backslash.
/// Returns the inner chars and the index after the closing quote.
fn consume_dquoted(c: &[char], i: usize) -> (Vec<char>, usize) {
    let mut inner = Vec::new();
    let mut j = i + 1;
    while j < c.len() && c[j] != '"' {
        if c[j] == '\\' && j + 1 < c.len() {
            match c[j + 1] {
                '$' | '`' | '"' | '\\' => {
                    inner.push(c[j + 1]);
                    j += 2;
                }
                '\n' => j += 2,
                _ => {
                    inner.push('\\');
                    inner.push(c[j + 1]);
                    j += 2;
                }
            }
        } else {
            inner.push(c[j]);
            j += 1;
        }
    }
    (inner, if j < c.len() { j + 1 } else { j })
}

/// Expand the inside of double quotes: `$` constructs and backticks are
/// active, everything else is literal. `None` = opaque.
fn expand_dquoted(c: &[char], ctx: &PushCtx) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '$' => {
                let (s, next) = expand_dollar(c, i, ctx)?;
                out.push_str(&s);
                i = next;
            }
            '`' => match find_closing_backtick(c, i + 1) {
                Some(j) => {
                    let body: String = c[i + 1..j].iter().collect();
                    out.push_str(&resolve_subst_output(&body)?);
                    i = j + 1;
                }
                None => {
                    out.push('`');
                    i += 1;
                }
            },
            _ => {
                out.push(c[i]);
                i += 1;
            }
        }
    }
    Some(out)
}

/// Expand one `$...` construct at `c[i] == '$'`. Returns the expansion and
/// the next index, or `None` when the value is statically unknowable.
fn expand_dollar(c: &[char], i: usize, ctx: &PushCtx) -> Option<(String, usize)> {
    if i + 1 >= c.len() {
        return Some(("$".to_string(), i + 1));
    }
    match c[i + 1] {
        '(' => {
            if i + 2 < c.len() && c[i + 2] == '(' {
                return None; // arithmetic: opaque
            }
            match close_paren(c, i + 2) {
                Some(j) => {
                    let body: String = c[i + 2..j].iter().collect();
                    Some((resolve_subst_output(&body)?, j + 1))
                }
                None => Some(("$".to_string(), i + 1)),
            }
        }
        '{' => match close_brace(c, i + 2) {
            Some(j) => {
                let inner: String = c[i + 2..j].iter().collect();
                Some((expand_brace_param(&inner, ctx)?, j + 1))
            }
            None => Some(("$".to_string(), i + 1)),
        },
        '\'' => {
            let (s, next) = ansi_c_quoted(c, i + 2);
            Some((s, next))
        }
        '"' => {
            // `$"..."`: locale string, no expansion inside.
            let mut j = i + 2;
            while j < c.len() && c[j] != '"' {
                if c[j] == '\\' {
                    j += 1;
                }
                j += 1;
            }
            let s: String = c[i + 2..j.min(c.len())].iter().collect();
            Some((s, if j < c.len() { j + 1 } else { j }))
        }
        _ => {
            let mut j = i + 1;
            if c[j].is_ascii_alphabetic() || c[j] == '_' {
                j += 1;
                while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '_') {
                    j += 1;
                }
                let name: String = c[i + 1..j].iter().collect();
                match ctx.vars.iter().find(|(n, _)| *n == name) {
                    Some((_, Some(v))) => Some((v.clone(), j)),
                    _ => None,
                }
            } else {
                None // `$?`, `$$`, `$1`, ...: opaque
            }
        }
    }
}

/// Expand `${name}`, `${name:-default}`, `${name-default}`,
/// `${name:=default}`, `${name=default}`. Anything else (`:+`, `:?`, `#`,
/// `%`, `/`, slicing, ...) is opaque.
fn expand_brace_param(inner: &str, ctx: &PushCtx) -> Option<String> {
    let bytes = inner.as_bytes();
    let mut k = 0;
    while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
        k += 1;
    }
    if k == 0 || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return None;
    }
    let name = &inner[..k];
    let rest = &inner[k..];
    if rest.is_empty() {
        return match ctx.vars.iter().find(|(n, _)| n == name) {
            Some((_, Some(v))) => Some(v.clone()),
            _ => None,
        };
    }
    let def = if let Some(d) = rest.strip_prefix(":-") {
        d
    } else if let Some(d) = rest.strip_prefix(":=") {
        d
    } else if let Some(d) = rest.strip_prefix('-') {
        d
    } else if let Some(d) = rest.strip_prefix('=') {
        d
    } else {
        return None;
    };
    // The default is itself shell text: expand recursively.
    match expand_seq(&def.chars().collect::<Vec<char>>(), ctx)? {
        alts if alts.len() == 1 => alts.into_iter().next(),
        _ => None,
    }
}

/// Interpret the inside of `$'...'` starting at `c[i]` (the char after
/// `$'`). Returns the value and the index after the closing quote.
fn ansi_c_quoted(c: &[char], i: usize) -> (String, usize) {
    let mut out = String::new();
    let mut j = i;
    while j < c.len() && c[j] != '\'' {
        if c[j] == '\\' && j + 1 < c.len() {
            j += 1;
            match c[j] {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'a' => out.push('\x07'),
                'b' => out.push('\x08'),
                'e' | 'E' => out.push('\x1b'),
                'f' => out.push('\x0c'),
                'v' => out.push('\x0b'),
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                '?' => out.push('?'),
                'x' => {
                    let mut val = 0u32;
                    for _ in 0..2 {
                        if j + 1 < c.len() && c[j + 1].is_ascii_hexdigit() {
                            j += 1;
                            val = val * 16 + c[j].to_digit(16).unwrap_or(0);
                        } else {
                            break;
                        }
                    }
                    out.push(char::from_u32(val).unwrap_or('\u{FFFD}'));
                }
                'u' => {
                    let mut val = 0u32;
                    for _ in 0..4 {
                        if j + 1 < c.len() && c[j + 1].is_ascii_hexdigit() {
                            j += 1;
                            val = val * 16 + c[j].to_digit(16).unwrap_or(0);
                        } else {
                            break;
                        }
                    }
                    out.push(char::from_u32(val).unwrap_or('\u{FFFD}'));
                }
                '0'..='7' => {
                    let mut val = c[j].to_digit(8).unwrap_or(0);
                    for _ in 1..3 {
                        if j + 1 < c.len() && matches!(c[j + 1], '0'..='7') {
                            j += 1;
                            val = val * 8 + c[j].to_digit(8).unwrap_or(0);
                        } else {
                            break;
                        }
                    }
                    out.push(char::from_u32(val).unwrap_or('\u{FFFD}'));
                }
                'c' => {
                    if j + 1 < c.len() {
                        j += 1;
                        let v = c[j].to_ascii_uppercase() as u32 & 0x1f;
                        out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                    }
                }
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
            j += 1;
        } else {
            out.push(c[j]);
            j += 1;
        }
    }
    (out, if j < c.len() { j + 1 } else { j })
}

/// Rewrite `$IFS` / `${IFS}` to spaces outside single quotes, mirroring
/// the shell's word splitting.
fn expand_ifs_words(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut sq = false;
    while i < c.len() {
        let ch = c[i];
        if sq {
            out.push(ch);
            if ch == '\'' {
                sq = false;
            }
            i += 1;
            continue;
        }
        if ch == '\'' {
            sq = true;
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '$' {
            let rest = &c[i + 1..];
            let bare = rest.starts_with(&['I', 'F', 'S'])
                && (rest.len() == 3 || {
                    let n = rest[3];
                    !(n.is_ascii_alphanumeric() || n == '_')
                });
            if bare {
                out.push(' ');
                i += 4;
                continue;
            }
            if rest.starts_with(&['{', 'I', 'F', 'S', '}']) {
                out.push(' ');
                i += 6;
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Best-effort static evaluation of a command substitution body's stdout,
/// without running anything. `None` for anything we cannot evaluate — the
/// caller treats unknown output conservatively.
///
/// Handled shapes (matched against the normalized body):
/// - `echo [-flags] words...` -> the words joined by one space
/// - `printf format [args...]` with no `%` in `format` -> format + args
/// - `which name` -> `name`
/// - `command -v|-V name` -> `name`
fn resolve_subst_output(body: &str) -> Option<String> {
    let body = normalize(body);
    let mut words = body.split_whitespace();
    match words.next()? {
        "echo" => {
            let args: Vec<&str> = words.filter(|w| !w.starts_with('-') || *w == "-").collect();
            Some(args.join(" "))
        }
        "printf" => {
            let fmt = words.next()?;
            if fmt.contains('%') {
                return None;
            }
            let mut out = fmt.to_string();
            for a in words {
                out.push_str(a);
            }
            Some(out)
        }
        "which" => words.next().map(|w| cmd_name(w).to_string()),
        "command" => match words.next()? {
            "-v" | "-V" => words.next().map(|w| cmd_name(w).to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// Top-level entry for one command string.
fn analyze_script(s: &str, ctx: &mut PushCtx, depth: usize) -> bool {
    if depth > 64 {
        return true; // pathological nesting: park it
    }
    // Every substitution / process-substitution body is itself a command
    // string and may push as a side effect (`echo $(git push)`).
    if scan_bodies(s, depth) {
        return true;
    }
    let toks = tokenize(s);
    let (hit, _) = scan_list(&toks, 0, toks.len(), ctx, depth, false);
    hit
}

/// Scan the raw command for substitution bodies (`$(...)`, backticks,
/// `<(...)`, `>(...)`), quote-aware, and analyze each body as its own
/// command string. Single quotes suppress substitution; double quotes do
/// not. `$((...))` arithmetic is skipped.
fn scan_bodies(s: &str, depth: usize) -> bool {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut q: Option<char> = None;
    while i < c.len() {
        let ch = c[i];
        if q == Some('\'') {
            if ch == '\'' {
                q = None;
            }
            i += 1;
            continue;
        }
        match ch {
            '\'' => {
                q = Some('\'');
                i += 1;
            }
            '"' if q.is_none() => {
                q = Some('"');
                i += 1;
            }
            '"' => {
                q = None;
                i += 1;
            }
            '\\' => i += 2,
            '`' => match find_closing_backtick(&c, i + 1) {
                Some(j) => {
                    let body: String = c[i + 1..j].iter().collect();
                    if analyze_script(&body, &mut PushCtx::new(), depth + 1) {
                        return true;
                    }
                    i = j + 1;
                }
                None => i += 1,
            },
            '$' if i + 1 < c.len() && c[i + 1] == '(' => {
                if i + 2 < c.len() && c[i + 2] == '(' {
                    i += 3; // arithmetic: no commands inside
                } else {
                    match close_paren(&c, i + 2) {
                        Some(j) => {
                            let body: String = c[i + 2..j].iter().collect();
                            if analyze_script(&body, &mut PushCtx::new(), depth + 1) {
                                return true;
                            }
                            i = j + 1;
                        }
                        None => i += 1,
                    }
                }
            }
            '<' | '>' if q.is_none() && i + 1 < c.len() && c[i + 1] == '(' => {
                match match_paren_chars(&c, i + 1) {
                    Some(end) => {
                        let body: String = c[i + 2..end - 1].iter().collect();
                        if analyze_script(&body, &mut PushCtx::new(), depth + 1) {
                            return true;
                        }
                        i = end;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    false
}

/// Index of the `)` balancing the `$(` whose body starts at `start` (the
/// char after `$(`), or `None` if unbalanced. Nesting-aware and
/// quote-aware: parens inside single or double quotes do not count.
fn close_paren(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut quote: Option<char> = None;
    let mut j = start;
    while j < chars.len() {
        let c = chars[j];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            },
        }
        j += 1;
    }
    None
}

/// Index of the backtick closing the pair opened before `start`, or `None`.
/// A backslash-escaped backtick does not close.
fn find_closing_backtick(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start;
    while j < chars.len() {
        if chars[j] == '\\' {
            j += 2;
            continue;
        }
        if chars[j] == '`' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Index of the `}` balancing the `${` whose body starts at `start`, or
/// `None` if unbalanced. Nesting-aware over `{...}`.
fn close_brace(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut j = start;
    while j < chars.len() {
        match chars[j] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Index just past the `)` matching the `(` at token `i`.
fn match_tok_paren(toks: &[Tok], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while j < toks.len() {
        match toks[j] {
            Tok::SubOpen(_) => depth += 1,
            Tok::SubClose => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Index just past the `}` matching the `{` at token `i`.
fn match_tok_brace(toks: &[Tok], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while j < toks.len() {
        match toks[j] {
            Tok::BraceOpen => depth += 1,
            Tok::BraceClose => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// True for words that terminate a command list (consumed by their keyword
/// handler, never a simple command word).
fn is_list_terminator(w: &str) -> bool {
    matches!(w, "then" | "do" | "done" | "fi" | "esac" | "elif" | "else")
}

/// True for words that open a compound construct (handled structurally).
fn is_compound_starter(w: &str) -> bool {
    matches!(
        w,
        "if" | "while" | "until" | "for" | "select" | "case" | "function"
    )
}

fn is_word(toks: &[Tok], i: usize, s: &str) -> bool {
    matches!(toks.get(i), Some(Tok::Word(w)) if w == s)
}

/// Scan a command list. Returns `(hit, next_index)`; stops without
/// consuming at `)`, `}`, `fi`, `done`, `esac`, `elif`, `else`, `then`,
/// `do` (and, inside case bodies, `)`/`}` are pattern text, not closers).
fn scan_list(
    toks: &[Tok],
    mut i: usize,
    end: usize,
    ctx: &mut PushCtx,
    depth: usize,
    in_case: bool,
) -> (bool, usize) {
    if depth > 128 {
        return (true, end); // pathological nesting: park it
    }
    while i < end {
        match &toks[i] {
            Tok::Newline
            | Tok::Semi
            | Tok::Amp
            | Tok::AndAnd
            | Tok::OrOr
            | Tok::Pipe
            | Tok::Bang => {
                i += 1;
            }
            Tok::SubClose | Tok::BraceClose => {
                if in_case {
                    i += 1; // case pattern `)`: not a group closer
                } else {
                    return (false, i);
                }
            }
            Tok::SubOpen(glued) => {
                if *glued {
                    // `(( ... ))` arithmetic command: cannot run commands
                    // itself (substitutions inside were pre-scanned).
                    i = match_tok_paren(toks, i).unwrap_or(end);
                } else {
                    match match_tok_paren(toks, i) {
                        Some(e) => {
                            let (hit, _) = scan_list(toks, i + 1, e - 1, ctx, depth + 1, false);
                            if hit {
                                return (true, e);
                            }
                            i = e;
                        }
                        None => return (false, end),
                    }
                }
            }
            Tok::BraceOpen => match match_tok_brace(toks, i) {
                Some(e) => {
                    let (hit, _) = scan_list(toks, i + 1, e - 1, ctx, depth + 1, false);
                    if hit {
                        return (true, e);
                    }
                    i = e;
                }
                None => return (false, end),
            },
            Tok::Redir => {
                i += 1;
                if i < end && matches!(toks[i], Tok::Word(_)) {
                    i += 1; // redirect target: a filename, never a command
                }
            }
            Tok::Word(w) => {
                let w = w.clone();
                if is_list_terminator(&w) {
                    return (false, i);
                }
                if w == "if" {
                    let (hit, ni) = scan_if(toks, i, end, ctx, depth);
                    if hit {
                        return (true, ni);
                    }
                    i = ni;
                } else if w == "while" || w == "until" {
                    let (hit, ni) = scan_while(toks, i, end, ctx, depth);
                    if hit {
                        return (true, ni);
                    }
                    i = ni;
                } else if w == "for" || w == "select" {
                    let (hit, ni) = scan_for(toks, i, end, ctx, depth);
                    if hit {
                        return (true, ni);
                    }
                    i = ni;
                } else if w == "case" {
                    let (hit, ni) = scan_case(toks, i, end, ctx, depth);
                    if hit {
                        return (true, ni);
                    }
                    i = ni;
                } else if w == "[[" {
                    // `[[ ... ]]` cannot run commands itself.
                    let mut j = i + 1;
                    while j < end && !is_word(toks, j, "]]") {
                        j += 1;
                    }
                    i = if j < end { j + 1 } else { i + 1 };
                } else if w == "function" {
                    i = scan_function_kw(toks, i, end, ctx, depth);
                } else if let Some((name, bi)) = fn_header(toks, i) {
                    i = scan_fn_body(toks, &name, bi, end, ctx, depth);
                } else {
                    let (hit, ni) = scan_simple(toks, i, end, ctx, depth);
                    if hit {
                        return (true, ni);
                    }
                    i = ni;
                }
            }
        }
    }
    (false, i)
}

/// `if cond; then body; [elif cond; then body;] [else body;] fi`.
fn scan_if(toks: &[Tok], i: usize, end: usize, ctx: &mut PushCtx, depth: usize) -> (bool, usize) {
    let mut i = i + 1;
    loop {
        let (hit, ni) = scan_list(toks, i, end, ctx, depth + 1, false);
        if hit {
            return (true, ni);
        }
        i = ni;
        if !is_word(toks, i, "then") {
            return (false, i);
        }
        i += 1;
        let (hit, ni) = scan_list(toks, i, end, ctx, depth + 1, false);
        if hit {
            return (true, ni);
        }
        i = ni;
        match toks.get(i) {
            Some(Tok::Word(w)) if w == "elif" => i += 1,
            Some(Tok::Word(w)) if w == "else" => {
                i += 1;
                let (hit, ni) = scan_list(toks, i, end, ctx, depth + 1, false);
                if hit {
                    return (true, ni);
                }
                i = ni;
                if is_word(toks, i, "fi") {
                    return (false, i + 1);
                }
                return (false, i);
            }
            Some(Tok::Word(w)) if w == "fi" => return (false, i + 1),
            _ => return (false, i),
        }
    }
}

/// `while|until cond; do body; done`.
fn scan_while(
    toks: &[Tok],
    i: usize,
    end: usize,
    ctx: &mut PushCtx,
    depth: usize,
) -> (bool, usize) {
    let mut i = i + 1;
    let (hit, ni) = scan_list(toks, i, end, ctx, depth + 1, false);
    if hit {
        return (true, ni);
    }
    i = ni;
    if !is_word(toks, i, "do") {
        return (false, i);
    }
    i += 1;
    let (hit, ni) = scan_list(toks, i, end, ctx, depth + 1, false);
    if hit {
        return (true, ni);
    }
    i = ni;
    if is_word(toks, i, "done") {
        i += 1;
    }
    (false, i)
}

/// `for ...; do body; done` / `select ...; do body; done`: skip the header
/// to `do`, then scan the body.
fn scan_for(toks: &[Tok], i: usize, end: usize, ctx: &mut PushCtx, depth: usize) -> (bool, usize) {
    let mut i = i + 1;
    while i < end {
        match &toks[i] {
            Tok::Word(w) if w == "do" => break,
            Tok::SubOpen(_) => i = match_tok_paren(toks, i).unwrap_or(end),
            Tok::BraceOpen => i = match_tok_brace(toks, i).unwrap_or(end),
            _ => i += 1,
        }
    }
    if i < end {
        i += 1; // past `do`
    }
    let (hit, ni) = scan_list(toks, i, end, ctx, depth + 1, false);
    if hit {
        return (true, ni);
    }
    i = ni;
    if is_word(toks, i, "done") {
        i += 1;
    }
    (false, i)
}

/// `case WORD in pat) body;; ... esac`.
fn scan_case(toks: &[Tok], i: usize, end: usize, ctx: &mut PushCtx, depth: usize) -> (bool, usize) {
    let mut i = i + 1;
    if i < end && matches!(toks[i], Tok::Word(_)) {
        i += 1; // the scrutinee word
    }
    if is_word(toks, i, "in") {
        i += 1;
    }
    let (hit, ni) = scan_list(toks, i, end, ctx, depth + 1, true);
    if hit {
        return (true, ni);
    }
    i = ni;
    if is_word(toks, i, "esac") {
        i += 1;
    }
    (false, i)
}

/// `name()`: returns the name and the index after `)`.
fn fn_header(toks: &[Tok], i: usize) -> Option<(String, usize)> {
    let name = match toks.get(i) {
        Some(Tok::Word(w)) if is_bare_name(w) => w.clone(),
        _ => return None,
    };
    match (toks.get(i + 1), toks.get(i + 2)) {
        (Some(Tok::SubOpen(_)), Some(Tok::SubClose)) => Some((name, i + 3)),
        _ => None,
    }
}

/// Scan a function body (`{ ...; }` or `( ... )`), recording the name when
/// the body pushes. The definition itself is not a push; the call is.
fn scan_fn_body(
    toks: &[Tok],
    name: &str,
    bi: usize,
    end: usize,
    ctx: &mut PushCtx,
    depth: usize,
) -> usize {
    let mut i = bi;
    let mut hit = false;
    if i < end && toks[i] == Tok::BraceOpen {
        if let Some(e) = match_tok_brace(toks, i) {
            let (h, _) = scan_list(toks, i + 1, e - 1, ctx, depth + 1, false);
            hit = h;
            i = e;
        } else {
            i = end;
        }
    } else if i < end && matches!(toks[i], Tok::SubOpen(_)) {
        if let Some(e) = match_tok_paren(toks, i) {
            let (h, _) = scan_list(toks, i + 1, e - 1, ctx, depth + 1, false);
            hit = h;
            i = e;
        } else {
            i = end;
        }
    }
    if hit {
        let lower = name.to_ascii_lowercase();
        if !ctx.push_fns.contains(&lower) {
            ctx.push_fns.push(lower);
        }
    }
    i
}

/// `function name [()] { ...; }`.
fn scan_function_kw(toks: &[Tok], i: usize, end: usize, ctx: &mut PushCtx, depth: usize) -> usize {
    let mut i = i + 1;
    let raw = match toks.get(i) {
        Some(Tok::Word(w)) => w.clone(),
        _ => return i,
    };
    let (name, had_parens) = match raw.strip_suffix("()") {
        Some(n) => (n.to_string(), true),
        None => (raw, false),
    };
    if !is_bare_name(&name) {
        return i;
    }
    i += 1;
    if !had_parens
        && i + 1 < end
        && matches!(toks[i], Tok::SubOpen(_))
        && toks[i + 1] == Tok::SubClose
    {
        i += 2;
    }
    scan_fn_body(toks, &name, i, end, ctx, depth)
}

/// Collect one simple command's raw words starting at `i` (redirection
/// targets are skipped: filenames are never commands). Returns
/// `(hit, next_index)`.
fn scan_simple(
    toks: &[Tok],
    i: usize,
    end: usize,
    ctx: &mut PushCtx,
    depth: usize,
) -> (bool, usize) {
    let mut words: Vec<String> = Vec::new();
    let mut j = i;
    while j < end {
        match &toks[j] {
            Tok::Word(w) => {
                if is_list_terminator(w) || is_compound_starter(w) || w == "[[" {
                    break;
                }
                words.push(w.clone());
                j += 1;
            }
            Tok::Redir => {
                j += 1;
                if j < end && matches!(toks[j], Tok::Word(_)) {
                    j += 1;
                }
            }
            _ => break,
        }
    }
    if words.is_empty() {
        return (false, j);
    }
    (analyze_simple(&words, ctx, depth), j)
}

/// True for `name`: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_bare_name(w: &str) -> bool {
    let mut ch = w.chars();
    matches!(ch.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && ch.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Split a leading `name=value` word. The value may be empty (`x=`).
fn split_assign(tok: &str) -> Option<(String, String)> {
    let (name, val) = tok.split_once('=')?;
    if !is_bare_name(name) {
        return None;
    }
    Some((name.to_string(), val.to_string()))
}

/// Statically expand an assignment value; `None` when opaque.
fn expand_assign_val(val: &str, ctx: &PushCtx) -> Option<String> {
    match expand_word(val, ctx) {
        WordVal::Known(a) if a.len() == 1 => a.into_iter().next(),
        _ => None,
    }
}

/// The static value of one word, or `None` when opaque or
/// brace-multiplied.
fn known1(word: &str, ctx: &PushCtx) -> Option<String> {
    match expand_word(word, ctx) {
        WordVal::Known(a) if a.len() == 1 => a.into_iter().next(),
        _ => None,
    }
}

/// Peel result for one wrapper layer.
enum Peel {
    /// Index of the next word to examine (the wrapped command).
    Next(usize),
    /// The wrapper provably does not execute its arguments
    /// (`command -v`).
    NoExec,
    /// Opaque wrapper arguments: park for the operator.
    Flag,
}

/// `sudo [flags] [VAR=x ...] [--] command`: skip flags (consuming each
/// flag's argument unexpanded) and env assignments.
fn peel_sudo(words: &[String], pos: usize, ctx: &PushCtx) -> Peel {
    let mut p = pos + 1;
    while p < words.len() {
        let v = match known1(&words[p], ctx) {
            Some(v) => v,
            None => return Peel::Flag,
        };
        if v == "--" {
            return Peel::Next((p + 1).min(words.len()));
        }
        if v.len() > 1 && v.starts_with('-') {
            let f = v.trim_start_matches('-');
            if f.len() == 1
                && matches!(
                    f.chars().next(),
                    Some('u' | 'g' | 'p' | 'r' | 't' | 'U' | 'D' | 'C' | 'R' | 'T' | 'a' | 'h')
                )
            {
                p = (p + 2).min(words.len()); // bare value-taking flag
            } else if matches!(
                f,
                "user" | "group" | "prompt" | "role" | "type" | "chdir" | "chroot" | "other-user"
            ) {
                p = (p + 2).min(words.len());
            } else {
                p += 1; // attached value (`-uroot`, `--user=root`) or valueless
            }
            continue;
        }
        if let Some(eq) = v.find('=') {
            if is_bare_name(&v[..eq]) {
                p += 1; // VAR=x passed into the command's environment
                continue;
            }
        }
        break;
    }
    Peel::Next(p)
}

/// `env [-i] [-u NAME] [VAR=x ...] [--] command`.
fn peel_env(words: &[String], pos: usize, ctx: &PushCtx) -> Peel {
    let mut p = pos + 1;
    while p < words.len() {
        let v = match known1(&words[p], ctx) {
            Some(v) => v,
            None => return Peel::Flag,
        };
        if v == "--" {
            return Peel::Next((p + 1).min(words.len()));
        }
        if v.len() > 1 && v.starts_with('-') {
            let f = v.trim_start_matches('-');
            if f.len() == 1 && matches!(f.chars().next(), Some('u' | 'C' | 'S' | 'f')) {
                p = (p + 2).min(words.len());
            } else if matches!(f, "unset" | "chdir" | "split-string" | "file") {
                p = (p + 2).min(words.len());
            } else {
                p += 1;
            }
            continue;
        }
        if let Some(eq) = v.find('=') {
            if is_bare_name(&v[..eq]) {
                p += 1;
                continue;
            }
        }
        break;
    }
    Peel::Next(p)
}

/// `timeout [flags] DURATION command`: skip flags, then exactly one
/// duration argument.
fn peel_timeout(words: &[String], pos: usize, ctx: &PushCtx) -> Peel {
    let mut p = pos + 1;
    while p < words.len() {
        let v = match known1(&words[p], ctx) {
            Some(v) => v,
            None => return Peel::Flag,
        };
        if v == "--" {
            p += 1;
            break;
        }
        if v.len() > 1 && v.starts_with('-') {
            let f = v.trim_start_matches('-');
            if f.len() == 1 && matches!(f.chars().next(), Some('s' | 'k')) {
                p = (p + 2).min(words.len());
            } else if matches!(f, "signal" | "kill-after") {
                p = (p + 2).min(words.len());
            } else {
                p += 1;
            }
            continue;
        }
        break;
    }
    if p < words.len() {
        p += 1; // the duration
    }
    Peel::Next(p)
}

/// `command [-p] [-v|-V] command`: `-v`/`-V` only describe, never execute.
fn peel_command(words: &[String], pos: usize, ctx: &PushCtx) -> Peel {
    let mut p = pos + 1;
    while p < words.len() {
        let v = match known1(&words[p], ctx) {
            Some(v) => v.to_ascii_lowercase(),
            None => return Peel::Flag,
        };
        if v == "-v" || v == "-V" {
            return Peel::NoExec;
        }
        if v == "--" {
            return Peel::Next((p + 1).min(words.len()));
        }
        if v.len() > 1 && v.starts_with('-') {
            p += 1;
            continue;
        }
        break;
    }
    Peel::Next(p)
}

/// `nice [-n N | -nN | -N] command`.
fn peel_nice(words: &[String], pos: usize, ctx: &PushCtx) -> Peel {
    let mut p = pos + 1;
    while p < words.len() {
        let v = match known1(&words[p], ctx) {
            Some(v) => v,
            None => return Peel::Flag,
        };
        if v == "--" {
            return Peel::Next((p + 1).min(words.len()));
        }
        if v.len() > 1 && v.starts_with('-') {
            if v == "-n" || v == "--adjustment" {
                p = (p + 2).min(words.len());
            } else {
                p += 1; // -n5, -5, --adjustment=5, valueless flags
            }
            continue;
        }
        break;
    }
    Peel::Next(p)
}

/// `stdbuf [-o N | -e N | -i N] command` (also `-o0`, `--output=0` forms).
fn peel_stdbuf(words: &[String], pos: usize, ctx: &PushCtx) -> Peel {
    let mut p = pos + 1;
    while p < words.len() {
        let v = match known1(&words[p], ctx) {
            Some(v) => v,
            None => return Peel::Flag,
        };
        if v == "--" {
            return Peel::Next((p + 1).min(words.len()));
        }
        if v.len() > 1 && v.starts_with('-') {
            if v == "-o" || v == "-e" || v == "-i" {
                p = (p + 2).min(words.len());
            } else {
                p += 1;
            }
            continue;
        }
        break;
    }
    Peel::Next(p)
}

/// Wrappers whose flags take no arguments: `setsid`, `nohup`, `time`,
/// `builtin`. (`time -p` is the only flag worth naming; it is valueless.)
fn peel_plain(words: &[String], pos: usize, ctx: &PushCtx) -> Peel {
    let mut p = pos + 1;
    while p < words.len() {
        let v = match known1(&words[p], ctx) {
            Some(v) => v,
            None => return Peel::Flag,
        };
        if v == "--" {
            return Peel::Next((p + 1).min(words.len()));
        }
        if v.len() > 1 && v.starts_with('-') {
            p += 1;
            continue;
        }
        break;
    }
    Peel::Next(p)
}

/// `exec [-c] [-l] [-a name] command`.
fn peel_exec(words: &[String], pos: usize, ctx: &PushCtx) -> Peel {
    let mut p = pos + 1;
    while p < words.len() {
        let v = match known1(&words[p], ctx) {
            Some(v) => v,
            None => return Peel::Flag,
        };
        if v == "--" {
            return Peel::Next((p + 1).min(words.len()));
        }
        if v == "-a" {
            p = (p + 2).min(words.len());
            continue;
        }
        if v.len() > 1 && v.starts_with('-') {
            p += 1;
            continue;
        }
        break;
    }
    Peel::Next(p)
}

/// `xargs [opts] [command [args...]]`: the command runs with its static
/// args plus unknown stdin-derived args appended. `-I R` / `--replace=R`
/// makes every `R` in the command words a stdin placeholder.
fn analyze_xargs(words: &[String], pos: usize, ctx: &mut PushCtx, depth: usize) -> bool {
    let mut p = pos + 1;
    // Replacement string for `-I` / `--replace` (`None` = no replacement).
    let mut replace: Option<String> = None;
    let mut replace_unknown = false;
    while p < words.len() {
        let raw = match known1(&words[p], ctx) {
            Some(v) => v,
            None => return true,
        };
        let v = raw.to_ascii_lowercase();
        if v == "--" {
            p += 1;
            break;
        }
        // `-I R` / `--replace[=]R`: capture the replacement string from the
        // RAW word — placeholders are case-sensitive (`-IQQ` ≠ `-Iqq`).
        if raw == "-I" || raw == "-i" || raw == "--replace" {
            match words.get(p + 1).and_then(|w| known1(w, ctx)) {
                Some(r) => replace = Some(r),
                None => replace_unknown = true,
            }
            p = (p + 2).min(words.len());
            continue;
        }
        if let Some(r) = raw.strip_prefix("-I").or_else(|| raw.strip_prefix("-i")) {
            // `-IR` attached; bare `-I` defaults to `{}`.
            replace = Some(if r.is_empty() {
                "{}".to_string()
            } else {
                r.to_string()
            });
            p += 1;
            continue;
        }
        if let Some(r) = raw.strip_prefix("--replace=") {
            replace = Some(r.to_string());
            p += 1;
            continue;
        }
        if v.len() > 1 && v.starts_with('-') {
            let f = v.trim_start_matches('-');
            let takes_val = (f.len() == 1
                && matches!(
                    f.chars().next(),
                    Some('a' | 'd' | 'E' | 'L' | 'n' | 'P' | 's')
                ))
                || matches!(
                    f,
                    "arg-file"
                        | "delimiter"
                        | "eof"
                        | "max-lines"
                        | "max-args"
                        | "max-procs"
                        | "max-chars"
                        | "process-slot-var"
                );
            if takes_val {
                p = (p + 2).min(words.len());
            } else {
                p += 1;
            }
            continue;
        }
        break;
    }
    if replace_unknown {
        return true;
    }
    if p >= words.len() {
        return false; // bare `xargs` echoes stdin
    }
    // A `-I R` placeholder anywhere in the command words is replaced by
    // stdin lines: the resulting command is unknowable.
    if let Some(r) = &replace {
        if words[p..].iter().any(|w| w.contains(r.as_str())) {
            return true;
        }
    }
    match expand_word(&words[p], ctx) {
        WordVal::Opaque => true,
        WordVal::Known(a) => {
            if a.len() == 1 && cmd_name(&a[0]).to_ascii_lowercase() == "git" {
                return subcommand_is_push_xargs(&words[p + 1..], ctx, replace.as_deref());
            }
            // Any other command: analyze it with its static args. Stdin
            // args are appended after them, so a static `sh -c "..."` (or
            // nested wrapper) still decides the outcome.
            analyze_simple(&words[p..], ctx, depth)
        }
    }
}

/// xargs variant of the git subcommand check: stdin appends args and `-I R`
/// placeholders are replaced by stdin lines, so a placeholder or a missing
/// static subcommand counts as a push. A static non-push subcommand word
/// fixes the subcommand (`git status <stdin-args>` never pushes).
fn subcommand_is_push_xargs(words: &[String], ctx: &PushCtx, replace: Option<&str>) -> bool {
    let mut i = 0;
    while i < words.len() {
        match expand_word(&words[i], ctx) {
            WordVal::Opaque => return true,
            WordVal::Known(a) => {
                let ls: Vec<String> = a.iter().map(|x| x.to_ascii_lowercase()).collect();
                if ls.iter().all(|x| x.len() > 1 && x.starts_with('-')) {
                    if ls.len() == 1 && GIT_GLOBAL_OPTS_WITH_VALUE.contains(&ls[0].as_str()) {
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                if let Some(r) = replace {
                    if ls.iter().any(|x| x.contains(r)) {
                        return true;
                    }
                }
                let is_push = ls.iter().any(|x| x == "push" || x == "push-options");
                if is_push && i + 1 < words.len() {
                    if let WordVal::Known(n) = expand_word(&words[i + 1], ctx) {
                        if n.iter().any(|x| x == "--help" || x == "-h") {
                            return false;
                        }
                    }
                }
                return is_push;
            }
        }
    }
    true // no static subcommand: stdin decides -> park
}

/// `eval` re-parses its arguments as shell code.
fn analyze_eval(words: &[String], ctx: &mut PushCtx, depth: usize) -> bool {
    let mut code = String::new();
    for w in words {
        match expand_word(w, ctx) {
            WordVal::Known(a) => {
                for alt in a {
                    if !code.is_empty() {
                        code.push(' ');
                    }
                    code.push_str(&alt);
                }
            }
            WordVal::Opaque => return true, // eval of unknown code: park it
        }
    }
    if code.trim().is_empty() {
        return false;
    }
    analyze_script(&code, &mut PushCtx::new(), depth + 1)
}

/// `sh|bash|dash|zsh [-c|--command] <code>`: scan the code as shell.
///
/// `-c` takes its code from the NEXT word (verified against bash/dash:
/// `-ce` runs the next word with `-e` as a flag; `-c<code>` attached is
/// rejected). A non-empty cluster remainder after `c` is still scanned as
/// code, conservatively, in case some shell accepts `-c<code>` attached.
fn analyze_sh_c(words: &[String], ctx: &mut PushCtx, depth: usize) -> bool {
    let mut i = 0;
    let mut expect_code = false;
    while i < words.len() {
        let raw = match known1(&words[i], ctx) {
            Some(v) => v,
            None => return true, // opaque word: park it
        };
        if expect_code {
            return match expand_word(&words[i], ctx) {
                WordVal::Known(a) if a.len() == 1 => analyze_script(
                    &a.into_iter().next().unwrap(),
                    &mut PushCtx::new(),
                    depth + 1,
                ),
                _ => true, // opaque or brace-multiplied code: park it
            };
        }
        let v = raw.to_ascii_lowercase();
        if v == "-c" || v == "--command" {
            expect_code = true;
            i += 1;
            continue;
        }
        if let Some(code) = raw.strip_prefix("--command=") {
            return analyze_script(code, &mut PushCtx::new(), depth + 1);
        }
        if v.starts_with('-') && v.len() > 1 && !v.starts_with("--") {
            // Short-flag cluster: `-c` may hide inside (`-ce`, `-ic`).
            // Byte indexes align: lowercasing is ASCII-only here.
            if let Some(ci) = v.find('c') {
                if ci + 1 < raw.len()
                    && analyze_script(&raw[ci + 1..], &mut PushCtx::new(), depth + 1)
                {
                    return true;
                }
                expect_code = true;
            }
            i += 1;
            continue;
        }
        if v.starts_with('-') {
            i += 1; // long flags
            continue;
        }
        return false; // non-option before -c: a script file, not code
    }
    false
}

/// True when the words after `git` select the push subcommand. An opaque
/// subcommand word counts as a push; with no words at all the answer is
/// `dangling` (false normally, true under xargs where stdin decides).
fn subcommand_is_push(words: &[String], ctx: &PushCtx, dangling: bool) -> bool {
    let mut i = 0;
    while i < words.len() {
        match expand_word(&words[i], ctx) {
            WordVal::Opaque => return true,
            WordVal::Known(a) => {
                let ls: Vec<String> = a.iter().map(|x| x.to_ascii_lowercase()).collect();
                if ls.iter().all(|x| x.len() > 1 && x.starts_with('-')) {
                    // Option word(s): skip, consuming the value of known
                    // value-taking git global options.
                    if ls.len() == 1 && GIT_GLOBAL_OPTS_WITH_VALUE.contains(&ls[0].as_str()) {
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                let is_push = ls.iter().any(|x| x == "push" || x == "push-options");
                if is_push && i + 1 < words.len() {
                    // `git push --help` / `git push -h` prints help and
                    // exits: it never pushes.
                    if let WordVal::Known(n) = expand_word(&words[i + 1], ctx) {
                        if n.iter().any(|x| x == "--help" || x == "-h") {
                            return false;
                        }
                    }
                }
                return is_push;
            }
        }
    }
    dangling
}

/// Analyze one simple command (raw words, redirections already removed).
fn analyze_simple(words: &[String], ctx: &mut PushCtx, depth: usize) -> bool {
    let mut pos = 0;
    // Leading `VAR=value` assignments (optional `export` prefix) are
    // tracked for `$VAR` / `${VAR}` expansion.
    while pos < words.len() {
        if words[pos] == "export" {
            if pos + 1 < words.len() {
                if let Some((n, v)) = split_assign(&words[pos + 1]) {
                    ctx.vars.push((n, expand_assign_val(&v, ctx)));
                    pos += 2;
                    continue;
                }
            }
            break;
        }
        match split_assign(&words[pos]) {
            Some((n, v)) => {
                ctx.vars.push((n, expand_assign_val(&v, ctx)));
                pos += 1;
            }
            None => break,
        }
    }
    if pos >= words.len() {
        return false;
    }
    // Peel wrapper layers; each layer either advances `pos` to the wrapped
    // command or decides the outcome outright.
    loop {
        if pos >= words.len() {
            return false;
        }
        let cmd = match expand_word(&words[pos], ctx) {
            WordVal::Known(a) if a.len() == 1 => {
                let w = a.into_iter().next().unwrap();
                cmd_name(&w).to_ascii_lowercase()
            }
            WordVal::Known(a) => {
                // Brace expansion in command position: the first
                // alternative is the command word, the rest become leading
                // arguments (`{git,hub} push` -> `git hub push`).
                let first = cmd_name(&a[0]).to_ascii_lowercase();
                if first == "git" {
                    let mut rest: Vec<String> = a[1..].to_vec();
                    rest.extend(words[pos + 1..].iter().cloned());
                    return subcommand_is_push(&rest, ctx, false);
                }
                if ctx.push_fns.iter().any(|f| f == &first) {
                    return true;
                }
                return false;
            }
            WordVal::Opaque => {
                // Unknown command word: it counts when a `push` (or
                // another opaque word) sits in argument position.
                return words[pos + 1..].iter().any(|w| match expand_word(w, ctx) {
                    WordVal::Opaque => true,
                    WordVal::Known(a) => a.iter().any(|x| {
                        let l = x.to_ascii_lowercase();
                        l == "push" || l == "push-options"
                    }),
                });
            }
        };
        match cmd.as_str() {
            "sudo" => match peel_sudo(words, pos, ctx) {
                Peel::Next(p) => pos = p,
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "env" => match peel_env(words, pos, ctx) {
                Peel::Next(p) => pos = p,
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "timeout" => match peel_timeout(words, pos, ctx) {
                Peel::Next(p) => pos = p,
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "command" => match peel_command(words, pos, ctx) {
                Peel::Next(p) => pos = p,
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "nice" => match peel_nice(words, pos, ctx) {
                Peel::Next(p) => pos = p,
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "stdbuf" => match peel_stdbuf(words, pos, ctx) {
                Peel::Next(p) => pos = p,
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "setsid" | "nohup" | "time" | "builtin" => match peel_plain(words, pos, ctx) {
                Peel::Next(p) => pos = p,
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "coproc" => match peel_plain(words, pos, ctx) {
                Peel::Next(p) => {
                    // `coproc [NAME] command`: the name may precede it.
                    if analyze_simple(&words[p.min(words.len())..], ctx, depth) {
                        return true;
                    }
                    let p2 = (p + 1).min(words.len());
                    if p2 < words.len() {
                        return analyze_simple(&words[p2..], ctx, depth);
                    }
                    return false;
                }
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "exec" => match peel_exec(words, pos, ctx) {
                Peel::Next(p) => pos = p,
                Peel::NoExec => return false,
                Peel::Flag => return true,
            },
            "xargs" => return analyze_xargs(words, pos, ctx, depth),
            "eval" => return analyze_eval(&words[pos + 1..], ctx, depth),
            "sh" | "bash" | "dash" | "zsh" => return analyze_sh_c(&words[pos + 1..], ctx, depth),
            "git" => return subcommand_is_push(&words[pos + 1..], ctx, false),
            _ => return ctx.push_fns.iter().any(|f| f == &cmd),
        }
    }
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
mod tests {
    use super::*;

    #[test]
    fn git_push_plain_command_detected() {
        assert!(is_git_push("git push origin main"));
    }

    #[test]
    fn git_push_after_semicolon_detected() {
        assert!(is_git_push("git fetch; git push"));
    }

    #[test]
    fn git_push_global_option_spellings_detected() {
        // The detector must skip git's global options to reach the
        // subcommand; stopping at the first token is a trivial bypass.
        assert!(is_git_push("git -C /tmp/repo push origin main"));
        assert!(is_git_push("git --git-dir=/tmp/repo/.git push"));
        assert!(is_git_push("git -c user.email=a@b push"));
    }

    #[test]
    fn git_push_in_dollar_paren_substitution_detected() {
        assert!(is_git_push("echo $(git push)"));
    }

    #[test]
    fn git_push_in_backtick_substitution_detected() {
        assert!(is_git_push(r#"echo `git push`"#));
    }

    #[test]
    fn git_push_in_nested_substitution_detected() {
        assert!(is_git_push("echo $(echo $(git push))"));
    }

    #[test]
    fn git_push_substitution_after_wrapper_detected() {
        assert!(is_git_push("sudo $(git push origin main)"));
    }

    #[test]
    fn git_push_in_brace_default_substitution_detected() {
        assert!(is_git_push("echo ${x:-$(git push)}"));
    }

    #[test]
    fn git_push_quoted_string_not_detected() {
        assert!(!is_git_push(r#"echo "git push""#));
    }

    #[test]
    fn git_push_single_quoted_string_not_detected() {
        assert!(!is_git_push("echo 'don't git push me'"));
    }

    #[test]
    fn git_push_quoted_inside_substitution_not_detected() {
        // The substitution runs, but its payload is just `echo` printing a
        // string — no git command executes.
        assert!(!is_git_push(r#"echo $(echo "git push")"#));
    }

    #[test]
    fn benign_substitution_not_detected() {
        assert!(!is_git_push("echo $(echo hello)"));
    }

    #[test]
    fn arithmetic_expansion_not_a_substitution() {
        assert!(!is_git_push("echo $((1 + 2))"));
    }

    // --- second-pass adversarial regression vectors (all must be pushes) ---

    #[test]
    fn git_push_substitution_in_argument_position() {
        // The substituted output lands in the subcommand slot.
        assert!(is_git_push("git $(echo push)"));
        assert!(is_git_push("git `echo push`"));
        assert!(is_git_push("git $(printf push) origin main"));
    }

    #[test]
    fn git_push_substitution_in_command_position() {
        // The substitution's output IS the command word.
        assert!(is_git_push("$(which git) push"));
        assert!(is_git_push("$(echo git) push origin main"));
    }

    #[test]
    fn git_push_unknown_command_position_is_conservative() {
        // Unresolvable command word: we cannot see what runs, so the
        // operator decides.
        assert!(is_git_push("$(get_git) push"));
    }

    #[test]
    fn git_push_eval_string_scanned_as_shell() {
        assert!(is_git_push(r#"eval "git push""#));
        assert!(is_git_push("eval 'git push origin main'"));
    }

    #[test]
    fn git_push_eval_benign_string_not_detected() {
        assert!(!is_git_push(r#"eval "echo hi""#));
    }

    #[test]
    fn git_push_quoted_paren_inside_substitution() {
        // The `)` inside the quoted string must not close the substitution.
        assert!(is_git_push(r#"$(echo " ) " ; git push)"#));
    }

    #[test]
    fn git_push_sh_dash_c_string_scanned_as_shell() {
        assert!(is_git_push(r#"sh -c "git push""#));
        assert!(is_git_push("bash -c 'git push origin main'"));
        assert!(is_git_push("sudo sh -c \"git push\""));
        // `-c` inside a flag cluster still takes the next word as code.
        assert!(is_git_push(r#"sh -ce "git push""#));
        assert!(is_git_push("bash --command='git push'"));
    }

    #[test]
    fn git_push_sh_dash_c_benign_not_detected() {
        assert!(!is_git_push(r#"sh -c "echo hi""#));
    }

    #[test]
    fn git_push_variable_indirection() {
        assert!(is_git_push("x=git; $x push"));
        assert!(is_git_push("x=git; ${x} push origin main"));
    }

    #[test]
    fn git_push_ifs_word_splitting() {
        assert!(is_git_push("git$IFS push"));
        assert!(is_git_push("git${IFS}push"));
    }

    #[test]
    fn git_push_single_quoted_substitution_is_inert() {
        // Single quotes suppress substitution: nothing executes.
        assert!(!is_git_push("echo '$(git push)'"));
    }

    #[test]
    fn git_push_double_quoted_substitution_executes() {
        // Double quotes do NOT suppress substitution: the push runs.
        assert!(is_git_push(r#"echo "$(git push)""#));
    }

    #[test]
    fn git_push_brace_default_in_argument_position() {
        assert!(is_git_push("git ${x:-$(echo push)}"));
    }

    #[test]
    fn git_push_unknown_subcommand_slot_is_conservative() {
        assert!(is_git_push("git $(mystery_cmd)"));
    }

    // --- round-3 structural bypass vectors (all must park approval) ---

    #[test]
    fn git_push_subshell() {
        assert!(is_git_push("(git push)"));
    }

    #[test]
    fn git_push_brace_group() {
        assert!(is_git_push("{ git push; }"));
    }

    #[test]
    fn git_push_if_branch() {
        assert!(is_git_push("if true; then git push; fi"));
    }

    #[test]
    fn git_push_function_definition_and_call() {
        assert!(is_git_push("g(){ git push;}; g"));
    }

    #[test]
    fn git_push_exec_prefix() {
        assert!(is_git_push("exec git push"));
    }

    #[test]
    fn git_push_negation() {
        assert!(is_git_push("! git push"));
    }

    #[test]
    fn git_push_until_loop() {
        assert!(is_git_push("until false; do git push; break; done"));
    }

    #[test]
    fn git_push_wrapper_arguments_peeled() {
        // The old strip-word approach never consumed wrapper ARGUMENTS,
        // so one flag defeated it every time.
        assert!(is_git_push("timeout 5 git push"));
        assert!(is_git_push("nice -n 5 git push"));
        assert!(is_git_push("sudo -u root git push"));
        assert!(is_git_push("env -i PATH=/usr/bin:/bin git push"));
        assert!(is_git_push("command -p git push"));
        assert!(is_git_push("stdbuf -o0 git push"));
        assert!(is_git_push("setsid git push"));
    }

    #[test]
    fn git_push_xargs_forms() {
        assert!(is_git_push("xargs -a /dev/null git push"));
        assert!(is_git_push("xargs git <<<push"));
        // `-I{}` placeholders are replaced by stdin lines.
        assert!(is_git_push("xargs -I{} git {} <<<push"));
        assert!(is_git_push("xargs -IQQ sh -c QQ"));
        assert!(!is_git_push("xargs -I{} git status"));
    }

    #[test]
    fn git_push_backslash_escapes() {
        assert!(is_git_push("\\git push"));
        assert!(is_git_push("g\\it push"));
        assert!(is_git_push("ba\\sh -c \"git push\""));
    }

    #[test]
    fn git_push_ansi_c_quoting() {
        assert!(is_git_push("git $'push'"));
        assert!(is_git_push("$'git' push"));
        assert!(is_git_push("x=$'git'; $x push"));
    }

    #[test]
    fn git_push_brace_expansion() {
        assert!(is_git_push("git p{u,}sh"));
        assert!(is_git_push("git {push,}"));
    }

    #[test]
    fn git_push_process_substitution() {
        assert!(is_git_push("echo hi > >(git push)"));
        assert!(is_git_push("cat <(git push)"));
    }

    #[test]
    fn git_push_line_continuation() {
        assert!(is_git_push("git \\\npush"));
    }

    #[test]
    fn git_push_brace_default_command_word() {
        assert!(is_git_push("${x:-git} push"));
    }

    // --- round-1 vectors: must stay closed ---

    #[test]
    fn git_push_round1_vectors_still_closed() {
        assert!(is_git_push("git $(echo push)"));
        assert!(is_git_push("git `echo push`"));
        assert!(is_git_push("$(which git) push"));
        assert!(is_git_push("$(echo git) push origin main"));
        assert!(is_git_push("$(get_git) push"));
        assert!(is_git_push("eval \"git push\""));
        assert!(is_git_push("sh -c \"git push\""));
        assert!(is_git_push("sudo sh -c \"git push\""));
    }

    // --- benign controls: must NOT park approval ---

    #[test]
    fn git_push_benign_controls_not_flagged() {
        for cmd in [
            "git status",
            "git log --oneline",
            "ls -la",
            "echo hi",
            "sudo apt update",
            "git fetch origin",
            "git diff HEAD",
            "git pull origin main",
            "echo \"just talking about git push\"",
            "cat README.md | grep push",
            "sh -c \"echo hi\"",
            "timeout 5 git status",
            "env -i git status",
            "xargs -a /dev/null git status",
            "git clone https://example.com/r.git",
            "git push --help",
        ] {
            assert!(!is_git_push(cmd), "false positive: {cmd}");
        }
    }
}
