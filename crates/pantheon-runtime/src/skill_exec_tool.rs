//! The `skill_exec` agent tool: run a skill's declared executables.
//!
//! Registration lives here (pantheon-runtime) because the tool registry
//! type belongs to pantheon-tools, which pantheon-exec cannot depend on
//! (Tools → Exec is the layering; Exec knows nothing of the registry).
//! All execution mechanics — parsing, confinement, sandboxing — stay in
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
/// Registered whenever at least one skill is installed — even when none
/// declares an executable — because the description carries the
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
/// read-side-effect executables need nothing more (they run like
/// read-only tools); write-side-effect ones additionally need
/// ShellExecute, so they flow through the normal approval path.
/// Unresolvable calls add nothing — the executor fails them closed.
fn extra_capabilities(skills: &[Skill], args: &str) -> Vec<Capability> {
    let Ok((skill, name, _)) = parse_skill_exec_args(args) else {
        return Vec::new();
    };
    match find_skill_exec(skills, &skill, &name) {
        Ok((_, exec)) if exec.side_effects == SkillExecSideEffects::Write => {
            vec![Capability::ShellExecute]
        }
        _ => Vec::new(),
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
         Read-side-effect executables run like read-only tools; write-side-effect ones \
         need ShellExecute and are blocked in Plan mode.",
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
                d.push_str(&format!(" — {}", e.description));
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
