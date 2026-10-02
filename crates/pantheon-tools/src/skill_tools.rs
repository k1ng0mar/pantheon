//! Skill tools: `skills_list` + `skill_read` on the shared registry.
//!
//! Moved out of `pantheon-exec::skills` in the tool-layer split
//! (capability ≠ tool): skill discovery/parse/import stays in
//! `pantheon-exec`; the callable surface lives here.

use pantheon_api::error::Layer;
use pantheon_exec::skills::{skill_body, Skill};
/// Register `skills_list` and `skill_read` on a registry.
///
/// `skills_list` is read-only (FilesystemRead): names + descriptions.
/// `skill_read` loads one skill's body, gated on FilesystemRead too
/// skills are data, not executable capability. A skill that wanted to
/// grant powers would be a plugin, not a skill.
pub fn register_skill_tools(reg: &mut crate::tools::ToolRegistry, skills: Vec<Skill>) {
    if skills.is_empty() {
        return;
    }
    let list = skills.clone();
    reg.register(
        pantheon_api::message::ToolSchema {
            name: "skills_list".into(),
            description: "List available skills (name, description, origin).".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
        pantheon_api::capability::Capability::FilesystemRead,
        move |_args| {
            let mut out = String::new();
            for s in &list {
                out.push_str(&format!(
                    "- {}: {} ({})\n",
                    s.meta.name, s.meta.description, s.meta.origin
                ));
            }
            Ok(out)
        },
    );

    let read = skills;
    reg.register(
        pantheon_api::message::ToolSchema {
            name: "skill_read".into(),
            description: "Read one skill's full markdown body by name.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        },
        pantheon_api::capability::Capability::FilesystemRead,
        move |args| {
            let v: serde_json::Value =
                serde_json::from_str(if args.trim().is_empty() { "{}" } else { args }).map_err(
                    |e| {
                        crate::tools::tool_err(
                            "TOOL_BAD_ARGS",
                            Layer::Execution,
                            false,
                            format!("invalid JSON args: {e}"),
                            "check tool name and arguments",
                        )
                    },
                )?;
            let name = v.get("name").and_then(|x| x.as_str()).ok_or_else(|| {
                crate::tools::tool_err(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    "missing 'name'".to_string(),
                    "check tool name and arguments",
                )
            })?;
            let skill = read.iter().find(|s| s.meta.name == name).ok_or_else(|| {
                crate::tools::tool_err(
                    "SKILL_UNKNOWN",
                    Layer::Execution,
                    false,
                    format!("no skill named '{name}'"),
                    "call skills_list first",
                )
            })?;
            skill_body(skill)
        },
    );
}
