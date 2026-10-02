//! Run skill-declared executables through the exec sandbox.
//!
//! `skill_exec` is the declared path for executing skill content: the
//! script (or npx spec) comes from the skill's `exec:` frontmatter,
//! arguments arrive as argv elements - never shell-interpolated - and the
//! run inherits the HIGH sandbox profile the `shell` tool uses, with the
//! wall clock set to the executable's declared `timeout_secs`.
//!
//! Third-party skills that never declared `exec:` (prose invocation like
//! `"${CLAUDE_SKILL_DIR}/scripts/..."`) are NOT run here; the agent runs
//! those via the regular `shell` tool after expanding the placeholder to
//! the skill's directory itself. To make the two worlds interoperate,
//! `skill_exec` still expands `${CLAUDE_SKILL_DIR}` / `${SKILL_DIR}` in
//! its own args, so a declared executable can accept skill-relative paths
//! written in the third-party convention.

use crate::sandbox::runner::run_sandboxed;
use crate::skills::{
    resolve_skill_exec_target, skill_dir, Skill, SkillExec, SkillExecRuntime, SkillExecTarget,
};
use crate::{SandboxLevel, SandboxProfile};
use pantheon_api::error::{Layer, PantheonError};
use std::path::Path;

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the skill's exec: declaration",
        "",
    )
}

/// Cap for one `skill_exec` run's captured stdout+stderr (256 KiB). The
/// sandbox runner already caps at 2 MiB; this is the tighter skill_exec
/// budget, applied after the run merges both streams.
pub const SKILL_EXEC_OUTPUT_CAP: usize = 256 * 1024;

/// Sandbox profile for a skill executable: HIGH isolation like `shell`,
/// wall clock from the declared `timeout_secs` (1..=600s, enforced at
/// parse time).
pub fn skill_exec_profile(exec: &SkillExec) -> SandboxProfile {
    let mut p = SandboxProfile::from(SandboxLevel::High);
    p.wall_clock_ms = exec.timeout_secs.saturating_mul(1000);
    p
}

/// Expand the de-facto skill-dir placeholders in one argument:
///
/// - `${CLAUDE_SKILL_DIR}` (Claude Code convention, used verbatim by
///   third-party skill docs)
/// - `${SKILL_DIR}` and `$SKILL_DIR` (short form)
///
/// Expansion is pure string substitution to the skill's absolute dir
/// the value still travels as a single argv element, never through a
/// shell, so this cannot widen the invocation. A bare `$SKILL_DIR` is
/// only expanded when not followed by `[A-Za-z0-9_]` (so `$SKILL_DIRECTORY`
/// is left alone).
pub fn expand_skill_dir_placeholders(arg: &str, dir: &Path) -> String {
    let dir_s = dir.to_string_lossy();
    // Braced forms first: exact tokens, no ambiguity.
    let out = arg
        .replace("${CLAUDE_SKILL_DIR}", dir_s.as_ref())
        .replace("${SKILL_DIR}", dir_s.as_ref());
    // Bare $SKILL_DIR: scan so a longer $SKILL_DIRSUFFIX is not clobbered.
    let token = "$SKILL_DIR";
    let mut expanded = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = rest.find(token) {
        let after = &rest[i + token.len()..];
        let boundary_ok = after
            .chars()
            .next()
            .map(|c| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(true);
        expanded.push_str(&rest[..i]);
        if boundary_ok {
            expanded.push_str(&dir_s);
        } else {
            expanded.push_str(token);
        }
        rest = after;
    }
    expanded.push_str(rest);
    expanded
}

/// Run one declared executable. Each argument becomes a separate argv
/// element after skill-dir placeholder expansion; nothing is
/// shell-interpolated. The working directory is the skill dir, so
/// scripts resolve bundled `scripts/` and `references/` relatively.
pub fn run_skill_exec(
    skill: &Skill,
    exec: &SkillExec,
    args: &[String],
) -> Result<String, PantheonError> {
    run_skill_exec_with(&skill_exec_profile(exec), skill, exec, args)
}

/// Inner run parameterized by profile so tests can use the InProcess
/// boundary (no bwrap on dev/CI machines) without weakening production,
/// which always uses [`skill_exec_profile`].
fn run_skill_exec_with(
    profile: &SandboxProfile,
    skill: &Skill,
    exec: &SkillExec,
    args: &[String],
) -> Result<String, PantheonError> {
    let who = format!("skill '{}', executable '{}'", skill.meta.name, exec.name);
    let target = resolve_skill_exec_target(skill, exec)?;
    let dir = skill_dir(skill);
    let expanded_args: Vec<String> = args
        .iter()
        .map(|a| expand_skill_dir_placeholders(a, &dir))
        .collect();
    let (program, argv): (String, Vec<String>) = match (&exec.runtime, &target) {
        (SkillExecRuntime::Shell, SkillExecTarget::Script(p)) => (
            "sh".to_string(),
            std::iter::once(p.to_string_lossy().to_string())
                .chain(expanded_args.iter().cloned())
                .collect(),
        ),
        (SkillExecRuntime::Python3, SkillExecTarget::Script(p)) => (
            "python3".to_string(),
            std::iter::once(p.to_string_lossy().to_string())
                .chain(expanded_args.iter().cloned())
                .collect(),
        ),
        (SkillExecRuntime::Node, SkillExecTarget::Script(p)) => (
            "node".to_string(),
            std::iter::once(p.to_string_lossy().to_string())
                .chain(expanded_args.iter().cloned())
                .collect(),
        ),
        (SkillExecRuntime::Npx, SkillExecTarget::Npx(spec)) => (
            "npx".to_string(),
            std::iter::once("-y".to_string())
                .chain(std::iter::once(spec.clone()))
                .chain(expanded_args.iter().cloned())
                .collect(),
        ),
        _ => {
            return Err(serr(
                "SKILL_EXEC_TARGET",
                format!("{who}: runtime/target mismatch (frontmatter changed after resolve?)"),
            ));
        }
    };
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let cwd = dir.to_string_lossy().to_string();
    let result = run_sandboxed(profile, &program, &argv_refs, &cwd)?;
    let code = result.exit_code;
    // Same honesty notice as the `shell` tool: a direct-spawn fallback
    // (explicitly opted in) must not be presented as sandboxed.
    let notice = if result.sandboxed {
        String::new()
    } else {
        "[sandbox unavailable on this host: ran WITHOUT namespace isolation]\n".to_string()
    };
    let output = cap_output(&result.output);
    // Exit code rides in the result text (the `shell` convention): the
    // model sees non-zero exits and can react, instead of the call
    // failing opaquely. Timeouts and spawn failures are real errors.
    let raw = if output.is_empty() {
        format!("(exit {code})")
    } else {
        format!("{output}\n(exit {code})")
    };
    Ok(format!("{notice}{raw}"))
}

/// Truncate captured output to [`SKILL_EXEC_OUTPUT_CAP`] bytes at a char
/// boundary, noting what was dropped.
fn cap_output(output: &str) -> String {
    if output.len() <= SKILL_EXEC_OUTPUT_CAP {
        return output.to_string();
    }
    let mut end = SKILL_EXEC_OUTPUT_CAP;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = output.len() - end;
    format!("{}[...truncated {dropped} bytes...]", &output[..end])
}
