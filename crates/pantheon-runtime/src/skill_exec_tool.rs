//! The `skill_exec` agent tool: run a skill's declared executables.
//!
//! Registration lives here (pantheon-runtime) because the tool registry
//! type belongs to pantheon-tools, which pantheon-exec cannot depend on
//! (Tools → Exec is the layering; Exec knows nothing of the registry).
//! All execution mechanics - parsing, confinement, sandboxing - stay in
//! pantheon-exec (`skills`, `skill_exec`).
//!
//! Two worlds, one tool list:
//! - Declared executables (`exec:` frontmatter) run via `skill_exec`:
//!   validated, sandboxed, attributed on the timeline.
//! - Third-party skills that invoke scripts through prose
//!   (`"${CLAUDE_SKILL_DIR}/scripts/..."`) are run by the agent via the
//!   regular `shell` tool. The `skill_exec` description therefore also
//!   lists every skill's absolute directory, so the agent can expand the
//!   placeholder itself.

use pantheon_api::capability::Capability;
use pantheon_api::error::PantheonError;
use pantheon_api::message::ToolSchema;
use pantheon_exec::skills::{
    find_skill_exec, parse_skill_exec_args, skill_dir, Skill, SkillExecSideEffects,
    SKILL_EXEC_TOOL_NAME,
};
use pantheon_tools::tools::ToolRegistry;
use std::sync::Arc;

/// Register `skill_exec` on a registry.
///
/// Registered whenever at least one skill is installed - even when none
/// declares an executable - because the description carries the
/// skill-directory mapping third-party prose-invoked scripts need. An
/// empty skill list registers nothing.
pub fn register_skill_exec_tool(reg: &mut ToolRegistry, skills: Vec<Skill>) {
    if skills.is_empty() {
        return;
    }
    let skills = Arc::new(skills);
    let for_caps = Arc::clone(&skills);
    let description = describe_skill_exec(&skills);
    reg.register_with(
        ToolSchema {
            name: SKILL_EXEC_TOOL_NAME.into(),
            description,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "Skill name, as listed by skills_list"
                    },
                    "name": {
                        "type": "string",
                        "description": "Executable name as declared in the skill's exec: frontmatter"
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Arguments, each passed as a separate argv element (never shell-interpolated). ${CLAUDE_SKILL_DIR} / $SKILL_DIR expand to the skill's directory."
                    }
                },
                "required": ["skill", "name"]
            }),
        },
        Capability::FilesystemRead,
        move |args| run_skill_exec_call(&skills, args),
        Some(Box::new(move |args: &str| {
            extra_capabilities(&for_caps, args)
        })),
    );
}

/// Capability gating per call, on top of the static FilesystemRead:
/// EVERY `skill_exec` run needs ShellExecute, whatever its declared
/// side effects say. The declaration is author-controlled, and a
/// read-declared executable is still arbitrary skill code running
/// under the HIGH sandbox (network on, no seccomp/landlock, whole
/// host tree readable) - strictly more than FilesystemRead, which
/// confines reads to the workspace. Trusting the declaration let a
/// Reader session run untrusted code and exfiltrate files the file
/// tools would confine-deny (e.g. ~/.ssh). With ShellExecute
/// required, a Reader policy denies the call, Plan mode refuses it
/// (see `skill_exec_call_is_mutating`), and under Coder it runs with
/// exactly the privilege the shell tool already grants - no new
/// privilege, no silent widening. Discovery-time scanning is not a
/// substitute: project-root skills (`.pantheon/skills`,
/// `.agents/skills`, `.claude/skills`, `PANTHEON_SKILLS_DIR`) are
/// collected without a SkillSpector gate, so the gate must live here,
/// at execution, not at import.
///
/// A call that cannot be resolved is treated as an execution attempt,
/// not as a read. The executor fails it closed, so nothing runs, but
/// the gate should not be the thing that decided it was harmless:
/// reporting `FilesystemRead` for a `skill_exec` invocation means a
/// caller that somehow got a resolvable exec past this check would
/// only need the read tier. Fail toward the capability that matches
/// the tool's nature.
fn extra_capabilities(skills: &[Skill], args: &str) -> Vec<Capability> {
    let Ok((skill, name, _)) = parse_skill_exec_args(args) else {
        // Not even parseable: still a `skill_exec` call.
        return vec![Capability::ShellExecute];
    };
    match find_skill_exec(skills, &skill, &name) {
        Ok(_) => vec![Capability::ShellExecute],
        // The model named a skill or an executable that does not exist.
        // Could be a stale prompt, could be probing for a gate that says
        // no. Either way this is an execution request.
        Err(_) => vec![Capability::ShellExecute],
    }
}

fn run_skill_exec_call(skills: &[Skill], args: &str) -> Result<String, PantheonError> {
    let (skill, name, exec_args) = parse_skill_exec_args(args)?;
    let (skill, exec) = find_skill_exec(skills, &skill, &name)?;
    pantheon_exec::skill_exec::run_skill_exec(skill, exec, &exec_args)
}

/// Tool description: declared executables (the `skill_exec` path) plus
/// the skill-directory mapping (the third-party prose path).
fn describe_skill_exec(skills: &[Skill]) -> String {
    let mut d = String::from(
        "Run one of a skill's declared executables (exec: frontmatter). \
         Each argument is passed as a separate argv element, never shell-interpolated; \
         ${CLAUDE_SKILL_DIR} / $SKILL_DIR in args expand to the skill's directory. \
         Every executable run needs ShellExecute, whatever its declared \\
         side effects say: a read-declared executable is still arbitrary \\
         skill code, not a read-only tool. All runs are blocked in Plan mode.",
    );
    let mut any = false;
    for s in skills {
        for e in s.meta.executables() {
            if !any {
                d.push_str("\n\nDeclared executables:");
                any = true;
            }
            d.push_str(&format!(
                "\n- skill {:?}, executable {:?}: {} [{}]",
                s.meta.name,
                e.name,
                if e.args.is_empty() {
                    "(no args)".to_string()
                } else {
                    e.args.clone()
                },
                e.side_effects.as_str(),
            ));
            if !e.description.is_empty() {
                d.push_str(&format!(" - {}", e.description));
            }
        }
    }
    if !any {
        d.push_str("\n\nNo skill currently declares an executable.");
    }
    d.push_str(
        "\n\nSkill directories (for third-party skills that invoke scripts through prose, \
         e.g. \"${CLAUDE_SKILL_DIR}/scripts/...\": expand the placeholder to the directory \
         below and run via the shell tool; skill_exec only runs exec:-declared entries):",
    );
    for s in skills {
        d.push_str(&format!(
            "\n- {:?}: {}",
            s.meta.name,
            skill_dir(s).display()
        ));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn read_declared_skill() -> Skill {
        let raw = "---\nname: probe\ndescription: test skill\nexec:\n  - name: probe-it\n    command: scripts/probe.sh\n    side_effects: read\n---\nBody.\n";
        pantheon_exec::skills::parse_skill(raw, Path::new("/tmp/probe/SKILL.md"))
            .expect("parse test skill")
    }

    #[test]
    fn read_declared_executable_still_needs_shell_execute() {
        // A `read` declaration is author-controlled: it must not drop
        // the call to the read tier, or a Reader session could run
        // arbitrary skill code under FilesystemRead alone.
        let skill = read_declared_skill();
        assert_eq!(
            skill.meta.exec[0].side_effects,
            SkillExecSideEffects::Read,
            "test fixture must declare read"
        );
        let caps = extra_capabilities(&[skill], r#"{"skill": "probe", "name": "probe-it"}"#);
        assert!(
            caps.contains(&Capability::ShellExecute),
            "read-declared skill_exec must require ShellExecute: {caps:?}"
        );
    }

    #[test]
    fn unresolvable_skill_exec_needs_shell_execute() {
        let skill = read_declared_skill();
        let caps = extra_capabilities(&[skill], r#"{"skill": "probe", "name": "nope"}"#);
        assert!(caps.contains(&Capability::ShellExecute));
    }
}
