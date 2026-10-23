//! User-defined slash commands.
//!
//! Two on-disk forms are supported, matching the familiar Markdown-first
//! workflow while keeping machine-generated command sets easy:
//!
//! * `.pantheon/commands/<name>.md` (project), `.opencode/commands/<name>.md`
//!   (compatibility), `<data_dir>/commands/<name>.md` (global). The body is
//!   the prompt; optional YAML-ish frontmatter supplies `description`,
//!   `agent`, and `model`.
//! * `commands.json` in any of those roots. It may be a map of names to
//!   `{ "description", "template"|"prompt", "agent", "model" }`, a single
//!   command object, or an array of command objects with `name`.
//!
//! `$ARGUMENTS` is replaced with the text after the command; `$1`..`$9` are
//! replaced with whitespace-separated arguments. If neither placeholder is
//! present, arguments are appended on a blank line. Built-in commands always
//! win over a user command with the same name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub template: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub source: PathBuf,
}

/// Discover commands from the project and global data roots. Project
/// commands shadow global ones; within one root, later candidates do not
/// replace the first definition. Invalid files are skipped rather than
/// making every other command unavailable.
pub fn discover(data_dir: &Path, project_root: &Path) -> Vec<CustomCommand> {
    let mut roots = vec![project_root.join(".pantheon/commands")];
    // OpenCode-compatible location. Keeping it read-only means a Pantheon
    // user never has to choose between the two ecosystems' command folders.
    roots.push(project_root.join(".opencode/commands"));
    roots.push(data_dir.join("commands"));
    if let Ok(extra) = std::env::var("PANTHEON_COMMANDS_DIR") {
        for entry in extra.split(':').filter(|s| !s.trim().is_empty()) {
            roots.push(PathBuf::from(entry));
        }
    }
    discover_in(&roots)
}

pub fn discover_in(roots: &[PathBuf]) -> Vec<CustomCommand> {
    let mut found: BTreeMap<String, CustomCommand> = BTreeMap::new();
    for root in roots {
        let Ok(read_dir) = std::fs::read_dir(root) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = read_dir
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .collect();
        entries.sort();
        for path in entries {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let parsed = match ext.as_str() {
                "md" => {
                    let stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_string();
                    parse_markdown(&path, &stem, &text).into_iter().collect()
                }
                "json" => parse_json(&path, &text),
                _ => Vec::new(),
            };
            for command in parsed {
                if command.name.is_empty() || crate::commands::is_builtin(&command.name) {
                    continue;
                }
                found.entry(command.name.clone()).or_insert(command);
            }
        }
    }
    found.into_values().collect()
}

pub fn find<'a>(commands: &'a [CustomCommand], name: &str) -> Option<&'a CustomCommand> {
    commands.iter().find(|c| c.name.eq_ignore_ascii_case(name))
}

/// Expand placeholders without invoking a shell. Arguments are data, never
/// a command line, so a command template cannot be escalated by user input.
pub fn expand(template: &str, arguments: &str) -> String {
    let args: Vec<&str> = arguments.split_whitespace().collect();
    let has_args_placeholder = template.contains("$ARGUMENTS");
    let mut positional = false;
    for i in 1..=9 {
        if template.contains(&format!("${i}")) || template.contains(&format!("${{{i}}}")) {
            positional = true;
            break;
        }
    }
    let mut out = template.replace("$ARGUMENTS", arguments.trim());
    for (i, arg) in args.iter().enumerate().take(9) {
        out = out.replace(&format!("${{{}}}", i + 1), arg);
        out = out.replace(&format!("${}", i + 1), arg);
    }
    for i in args.len() + 1..=9 {
        out = out.replace(&format!("${{{}}}", i), "");
        out = out.replace(&format!("${}", i), "");
    }
    if !has_args_placeholder && !positional && !arguments.trim().is_empty() {
        out.push_str("\n\n");
        out.push_str(arguments.trim());
    }
    out.trim().to_string()
}

fn parse_markdown(path: &Path, stem: &str, text: &str) -> Option<CustomCommand> {
    let (meta, body) = split_frontmatter(text);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let name = meta
        .get("name")
        .map(String::as_str)
        .unwrap_or(stem)
        .trim()
        .trim_start_matches('/')
        .to_string();
    let description = meta
        .get("description")
        .cloned()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| first_line(body));
    Some(CustomCommand {
        name,
        description,
        template: body.to_string(),
        agent: meta.get("agent").cloned().filter(|s| !s.is_empty()),
        model: meta.get("model").cloned().filter(|s| !s.is_empty()),
        source: path.to_path_buf(),
    })
}

fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut meta = BTreeMap::new();
    let rest = if let Some(rest) = text.strip_prefix("---\n") {
        rest
    } else if let Some(rest) = text.strip_prefix("---\r\n") {
        rest
    } else {
        return (meta, text);
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let clean = line.trim_end_matches(['\r', '\n']);
        if clean.trim() == "---" {
            let body = &rest[offset + line.len()..];
            return (meta, body);
        }
        if let Some((key, value)) = clean.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            if !key.is_empty() {
                meta.insert(key, unquote(value.trim()));
            }
        }
        offset += line.len();
    }
    // No closing fence: treat the document as ordinary Markdown, not as a
    // command with an empty body.
    (BTreeMap::new(), text)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn first_line(body: &str) -> String {
    body.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().trim_start_matches('#').trim().to_string())
        .unwrap_or_default()
}

fn parse_json(path: &Path, text: &str) -> Vec<CustomCommand> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("command");
    let mut out = Vec::new();
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(command) = json_command(path, stem, None, &item) {
                    out.push(command);
                }
            }
        }
        serde_json::Value::Object(map) => {
            if map.contains_key("template") || map.contains_key("prompt") {
                if let Some(command) =
                    json_command(path, stem, None, &serde_json::Value::Object(map))
                {
                    out.push(command);
                }
            } else {
                let nested = map.get("commands").and_then(|v| v.as_object());
                let entries: Box<dyn Iterator<Item = (&String, &serde_json::Value)>> =
                    if let Some(commands) = nested {
                        Box::new(commands.iter())
                    } else {
                        Box::new(map.iter())
                    };
                for (name, item) in entries {
                    if let Some(command) = json_command(path, stem, Some(name), item) {
                        out.push(command);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn json_command(
    path: &Path,
    stem: &str,
    key: Option<&str>,
    value: &serde_json::Value,
) -> Option<CustomCommand> {
    let (template, description, name, agent, model) = match value {
        serde_json::Value::String(template) => (
            template.clone(),
            String::new(),
            key.unwrap_or(stem).to_string(),
            None,
            None,
        ),
        serde_json::Value::Object(map) => {
            let template = map
                .get("template")
                .or_else(|| map.get("prompt"))
                .and_then(|v| v.as_str())?
                .to_string();
            let description = map
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = map
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| key.unwrap_or(stem))
                .trim_start_matches('/')
                .to_string();
            (
                template,
                description,
                name,
                map.get("agent")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                map.get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            )
        }
        _ => return None,
    };
    if template.trim().is_empty() || name.is_empty() {
        return None;
    }
    Some(CustomCommand {
        name,
        description: if description.trim().is_empty() {
            first_line(&template)
        } else {
            description
        },
        template,
        agent,
        model,
        source: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_frontmatter_and_body_are_parsed() {
        let text = "---\ndescription: Review a diff\nagent: coder\n---\nReview this:\n$ARGUMENTS\n";
        let cmd = parse_markdown(Path::new("review.md"), "review", text).unwrap();
        assert_eq!(cmd.name, "review");
        assert_eq!(cmd.description, "Review a diff");
        assert_eq!(cmd.agent.as_deref(), Some("coder"));
        assert_eq!(cmd.template, "Review this:\n$ARGUMENTS");
    }

    #[test]
    fn no_closing_frontmatter_is_plain_markdown() {
        let cmd = parse_markdown(Path::new("x.md"), "x", "---\nnot: frontmatter\nhello").unwrap();
        assert!(cmd.template.contains("not: frontmatter"));
        assert_eq!(cmd.description, "---");
    }

    #[test]
    fn arguments_replace_placeholders_and_append_when_absent() {
        assert_eq!(expand("Review $ARGUMENTS", "a.rs"), "Review a.rs");
        assert_eq!(expand("Compare $1 with $2", "a b"), "Compare a with b");
        assert_eq!(expand("Do the thing", "now"), "Do the thing\n\nnow");
        assert_eq!(expand("Do it", ""), "Do it");
    }

    #[test]
    fn json_map_and_single_object_are_supported() {
        let map = r#"{"review":{"description":"Review","template":"Look at $ARGUMENTS"}}"#;
        let cmds = parse_json(Path::new("commands.json"), map);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "review");
        assert_eq!(cmds[0].template, "Look at $ARGUMENTS");

        let one = r#"{"name":"explain","prompt":"Explain $1"}"#;
        let cmds = parse_json(Path::new("one.json"), one);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "explain");
    }

    #[test]
    fn project_roots_shadow_global_roots() {
        let nonce = format!(
            "pantheon-commands-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let project = std::env::temp_dir().join(format!("{nonce}-project"));
        let global = std::env::temp_dir().join(format!("{nonce}-global"));
        std::fs::create_dir_all(project.join(".pantheon/commands")).unwrap();
        std::fs::create_dir_all(global.join("commands")).unwrap();
        std::fs::write(
            project.join(".pantheon/commands/review.md"),
            "project prompt",
        )
        .unwrap();
        std::fs::write(global.join("commands/review.md"), "global prompt").unwrap();
        let cmds = discover(&global, &project);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].template, "project prompt");
        let _ = std::fs::remove_dir_all(&project);
        let _ = std::fs::remove_dir_all(&global);
    }
}
