//! Skill discovery: `SKILL.md` files with YAML frontmatter are the
//! capability contract (ECC/OpenMausBot pattern). Each skill is a named,
//! described, self-contained unit the model can list and load.
//!
//! Discovery scans these places:
//!   <data_dir>/skills/<name>/SKILL.md
//!   <cwd>/.pantheon/skills/<name>/SKILL.md
//!
//! Skills are data, not code: the model sees name + description, and
//! reads the body via a gated tool. Nothing executes at discovery time.
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the SKILL.md",
        "",
    )
}

/// Parsed frontmatter of one skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Origin tag for provenance (e.g. "bundled", "user", catalog name).
    #[serde(default)]
    pub origin: String,
}

/// One discovered skill: metadata plus where its body lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub meta: SkillMeta,
    /// Absolute path to the SKILL.md file.
    pub path: PathBuf,
}

/// Parse frontmatter + body from a SKILL.md. Frontmatter is `---` fenced
/// YAML at the very top. Missing frontmatter is an error; an empty body
/// is fine.
pub fn parse_skill(raw: &str, path: &Path) -> Result<Skill, PantheonError> {
    let rest = raw.strip_prefix("---").ok_or_else(|| {
        serr(
            "SKILL_NO_FRONTMATTER",
            format!("{}: no frontmatter block", path.display()),
        )
    })?;
    let end = rest.find("\n---").ok_or_else(|| {
        serr(
            "SKILL_NO_FRONTMATTER",
            format!("{}: frontmatter not closed", path.display()),
        )
    })?;
    let yaml = &rest[..end];
    let mut meta: SkillMeta = serde_yaml::from_str(yaml)
        .map_err(|e| serr("SKILL_BAD_FRONTMATTER", format!("{}: {e}", path.display())))?;
    if meta.name.trim().is_empty() {
        return Err(serr(
            "SKILL_NO_NAME",
            format!("{}: frontmatter has no name", path.display()),
        ));
    }
    if meta.origin.is_empty() {
        meta.origin = "user".into();
    }
    Ok(Skill {
        meta,
        path: path.to_path_buf(),
    })
}

/// Load one skill from a SKILL.md path. Broken skills are skipped (None).
pub fn load_skill(path: &Path) -> Option<Skill> {
    match std::fs::read_to_string(path) {
        Ok(raw) => match parse_skill(&raw, path) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("skill {}: {e}", path.display());
                None
            }
        },
        Err(_) => None,
    }
}

/// Discover skills from the data dir and project root. Dedup by skill
/// name (first wins: user scope beats project scope on collision).
pub fn discover_skills(data_dir: &Path, project_root: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for base in [
        data_dir.join("skills"),
        project_root.join(".pantheon").join("skills"),
    ] {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for e in entries.flatten() {
            let candidate = e.path().join("SKILL.md");
            if candidate.is_file() {
                if let Some(s) = load_skill(&candidate) {
                    if !out.iter().any(|x| x.meta.name == s.meta.name) {
                        out.push(s);
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    out
}

/// Read a skill's full markdown body (frontmatter stripped).
pub fn skill_body(skill: &Skill) -> Result<String, PantheonError> {
    let raw = std::fs::read_to_string(&skill.path)
        .map_err(|e| serr("SKILL_READ", format!("{}: {e}", skill.path.display())))?;
    let rest = raw.strip_prefix("---").unwrap_or(&raw);
    match rest.find("\n---") {
        Some(end) => Ok(rest[end + 4..].trim_start().to_string()),
        None => Ok(raw),
    }
}

/// Register `skills_list` and `skill_read` on a registry.
///
/// `skills_list` is read-only (FilesystemRead): names + descriptions.
/// `skill_read` loads one skill's body, gated on FilesystemRead too —
/// skills are data, not executable capability. A skill that wanted to
/// grant powers would be a plugin, not a skill.
pub fn register_skill_tools(reg: &mut crate::tools::ToolRegistry, skills: Vec<Skill>) {
    if skills.is_empty() {
        return;
    }
    let list = skills.clone();
    reg.register(
        pantheon_core::message::ToolSchema {
            name: "skills_list".into(),
            description: "List available skills (name, description, origin).".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
        pantheon_core::capability::Capability::FilesystemRead,
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
        pantheon_core::message::ToolSchema {
            name: "skill_read".into(),
            description: "Read one skill's full markdown body by name.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        },
        pantheon_core::capability::Capability::FilesystemRead,
        move |args| {
            let v: serde_json::Value =
                serde_json::from_str(if args.trim().is_empty() { "{}" } else { args }).map_err(
                    |e| {
                        PantheonError::new(
                            "TOOL_BAD_ARGS",
                            Layer::Execution,
                            false,
                            format!("invalid JSON args: {e}"),
                            "check tool name and arguments",
                            "",
                        )
                    },
                )?;
            let name = v.get("name").and_then(|x| x.as_str()).ok_or_else(|| {
                PantheonError::new(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    "missing 'name'".to_string(),
                    "check tool name and arguments",
                    "",
                )
            })?;
            let skill = read.iter().find(|s| s.meta.name == name).ok_or_else(|| {
                PantheonError::new(
                    "SKILL_UNKNOWN",
                    Layer::Execution,
                    false,
                    format!("no skill named '{name}'"),
                    "call skills_list first",
                    "",
                )
            })?;
            skill_body(skill)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_dir(base: &Path, name: &str, desc: &str) -> PathBuf {
        let d = base.join(name);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("SKILL.md");
        std::fs::write(
            &p,
            format!(
                "---\nname: {name}\ndescription: \"{desc}\"\n---\n\n# {name}\n\nBody of {name}.\n"
            ),
        )
        .unwrap();
        p
    }

    #[test]
    fn parse_rejects_missing_frontmatter() {
        let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 1));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("SKILL.md");
        std::fs::write(&p, "# no frontmatter").unwrap();
        let err = parse_skill("# no frontmatter", &p).unwrap_err();
        assert_eq!(err.code, "SKILL_NO_FRONTMATTER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_finds_both_scopes_and_dedups() {
        let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 2));
        let data = dir.join("data");
        let proj = dir.join("proj");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        skill_dir(&data.join("skills"), "alpha", "first");
        skill_dir(&proj.join(".pantheon").join("skills"), "beta", "second");
        let found = discover_skills(&data, &proj);
        let names: Vec<&str> = found.iter().map(|s| s.meta.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn body_strips_frontmatter() {
        let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 3));
        std::fs::create_dir_all(&dir).unwrap();
        let p = skill_dir(&dir, "gamma", "d");
        let s = load_skill(&p).unwrap();
        let body = skill_body(&s).unwrap();
        assert!(body.starts_with("# gamma"), "{body}");
        assert!(!body.contains("description"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_tools_list_and_read() {
        let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 4));
        std::fs::create_dir_all(&dir).unwrap();
        skill_dir(&dir, "delta", "the delta skill");
        let s = load_skill(&dir.join("delta").join("SKILL.md")).unwrap();
        let mut reg = crate::tools::ToolRegistry::new();
        register_skill_tools(&mut reg, vec![s]);
        let listed = reg.execute("skills_list", "{}").unwrap();
        assert!(listed.contains("delta"), "{listed}");
        assert!(listed.contains("the delta skill"), "{listed}");
        let body = reg.execute("skill_read", r#"{"name":"delta"}"#).unwrap();
        assert!(body.starts_with("# delta"), "{body}");
        let err = reg.execute("skill_read", r#"{"name":"nope"}"#).unwrap_err();
        assert_eq!(err.code, "SKILL_UNKNOWN");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
