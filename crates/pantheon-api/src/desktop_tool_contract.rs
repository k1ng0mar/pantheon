//! Stable contract for tools exposed by the first party desktop client.
//!
//! Desktop supplies UI and native capability adapters. Pantheon owns the tool
//! names, their domains and the rule that a catalog may only contain known
//! Pantheon tools. This module is intentionally small and dependency free so
//! both the workspace and the separately built desktop can use it.

use std::collections::BTreeSet;

/// Prefix required for every agent callable desktop adapter tool.
pub const DESKTOP_TOOL_PREFIX: &str = "pantheon_";

/// Coarse domain used by tests and capability mapping. Runtime policy remains
/// the authority for what an operation may do; this is not a permission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopToolDomain {
    Project,
    Task,
    Browser,
    Files,
    Terminal,
    Team,
    Bot,
    Device,
    Design,
    Cloud,
    Testing,
    Metadata,
}

impl DesktopToolDomain {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Task => "task",
            Self::Browser => "browser",
            Self::Files => "files",
            Self::Terminal => "terminal",
            Self::Team => "team",
            Self::Bot => "bot",
            Self::Device => "device",
            Self::Design => "design",
            Self::Cloud => "cloud",
            Self::Testing => "testing",
            Self::Metadata => "metadata",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopTool {
    pub name: &'static str,
    pub domain: DesktopToolDomain,
}

/// All agent callable tools the desktop may currently expose.
///
/// Adding a name here is a deliberate API change and must be accompanied by
/// the native Pantheon tool contract it maps to, or by an explicit desktop UI
/// adapter justification in `docs/developer/desktop-tool-ownership.md`.
pub const DESKTOP_TOOLS: &[DesktopTool] = &[
    desktop("list_projects", DesktopToolDomain::Project),
    desktop("list_tasks", DesktopToolDomain::Task),
    desktop("read_task", DesktopToolDomain::Task),
    desktop("list_moodboards", DesktopToolDomain::Project),
    desktop("read_moodboard", DesktopToolDomain::Project),
    desktop("read_moodboard_image", DesktopToolDomain::Project),
    desktop("list_subagents", DesktopToolDomain::Task),
    desktop("create_task", DesktopToolDomain::Task),
    desktop("start_task", DesktopToolDomain::Task),
    desktop("send_message", DesktopToolDomain::Task),
    desktop("rename_task", DesktopToolDomain::Task),
    desktop("search_tasks", DesktopToolDomain::Task),
    desktop("code_search", DesktopToolDomain::Files),
    desktop("list_queue", DesktopToolDomain::Task),
    desktop("queue_message", DesktopToolDomain::Task),
    desktop("remove_queued", DesktopToolDomain::Task),
    desktop("steer_task", DesktopToolDomain::Task),
    desktop("interrupt_task", DesktopToolDomain::Task),
    desktop("pin_task", DesktopToolDomain::Task),
    desktop("archive_task", DesktopToolDomain::Task),
    desktop("set_task_stage", DesktopToolDomain::Task),
    desktop("create_worktree", DesktopToolDomain::Files),
    desktop("open_task", DesktopToolDomain::Task),
    desktop("open_file", DesktopToolDomain::Files),
    desktop("open_terminal", DesktopToolDomain::Terminal),
    desktop("list_local_servers", DesktopToolDomain::Project),
    desktop("open_browser", DesktopToolDomain::Browser),
    desktop("read_browser", DesktopToolDomain::Browser),
    desktop("browser_action", DesktopToolDomain::Browser),
    desktop("browser_click", DesktopToolDomain::Browser),
    desktop("browser_type", DesktopToolDomain::Browser),
    desktop("capture_browser", DesktopToolDomain::Browser),
    desktop("run_project", DesktopToolDomain::Terminal),
    desktop("preview_file", DesktopToolDomain::Browser),
    desktop("jev_test", DesktopToolDomain::Testing),
    desktop("jev_read", DesktopToolDomain::Testing),
    desktop("jev_wait", DesktopToolDomain::Testing),
    desktop("jev_list", DesktopToolDomain::Testing),
    desktop("jev_screenshot", DesktopToolDomain::Testing),
    desktop("jev_cancel", DesktopToolDomain::Testing),
    desktop("jev_review", DesktopToolDomain::Testing),
    desktop("jev_check_rules", DesktopToolDomain::Testing),
    desktop("jev_find_context", DesktopToolDomain::Testing),
    desktop("jev_choose_worker", DesktopToolDomain::Testing),
    desktop("jev_assessments", DesktopToolDomain::Testing),
    desktop("dispatch_workers", DesktopToolDomain::Team),
    desktop("list_workers", DesktopToolDomain::Team),
    desktop("read_worker", DesktopToolDomain::Team),
    desktop("followup_worker", DesktopToolDomain::Team),
    desktop("cancel_worker", DesktopToolDomain::Team),
    desktop("finish_team", DesktopToolDomain::Team),
    desktop("team_directory", DesktopToolDomain::Team),
    desktop("send_team_message", DesktopToolDomain::Team),
    desktop("read_team_mailbox", DesktopToolDomain::Team),
    desktop("checkpoint_worker", DesktopToolDomain::Team),
    desktop("title", DesktopToolDomain::Metadata),
    desktop("track_files", DesktopToolDomain::Files),
    desktop("design_list", DesktopToolDomain::Design),
    desktop("design_write", DesktopToolDomain::Design),
    desktop("design_comments", DesktopToolDomain::Design),
    desktop("design_resolve", DesktopToolDomain::Design),
    desktop("bot_remember", DesktopToolDomain::Bot),
    desktop("bot_forget", DesktopToolDomain::Bot),
    desktop("bot_recall", DesktopToolDomain::Bot),
    desktop("bot_schedule", DesktopToolDomain::Bot),
    desktop("bot_list_schedules", DesktopToolDomain::Bot),
    desktop("bot_cancel_schedule", DesktopToolDomain::Bot),
    desktop("bot_notify", DesktopToolDomain::Bot),
    desktop("simulator_devices", DesktopToolDomain::Device),
    desktop("simulator_screenshot", DesktopToolDomain::Device),
    desktop("simulator_describe", DesktopToolDomain::Device),
    desktop("simulator_control", DesktopToolDomain::Device),
    desktop("simulator_manage", DesktopToolDomain::Device),
    desktop("vm_list", DesktopToolDomain::Device),
    desktop("vm_screenshot", DesktopToolDomain::Device),
    desktop("vm_control", DesktopToolDomain::Device),
    desktop("vm_exec", DesktopToolDomain::Device),
    desktop("vm_manage", DesktopToolDomain::Device),
    desktop("cloud_cli", DesktopToolDomain::Cloud),
];

const fn desktop(suffix: &'static str, domain: DesktopToolDomain) -> DesktopTool {
    DesktopTool {
        name: suffix,
        domain,
    }
}

/// Full tool name for a contract entry.
pub fn full_name(tool: DesktopTool) -> String {
    format!("{DESKTOP_TOOL_PREFIX}{}", tool.name)
}

/// True when `name` is a known first party desktop tool.
pub fn is_desktop_tool(name: &str) -> bool {
    DESKTOP_TOOLS
        .iter()
        .any(|tool| full_name(*tool) == name)
}

/// Domain of a known desktop tool.
pub fn domain_of(name: &str) -> Option<DesktopToolDomain> {
    DESKTOP_TOOLS
        .iter()
        .find(|tool| full_name(**tool) == name)
        .map(|tool| tool.domain)
}

/// Validate a JSON tool catalog before it can be exposed to the agent.
pub fn validate_catalog(tools: &[serde_json::Value]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for tool in tools {
        if tool["type"] != "function" {
            return Err("Desktop tool catalog accepts only function tools".into());
        }
        let name = tool["name"]
            .as_str()
            .ok_or("Desktop tool is missing a name")?;
        let canonical = name
            .strip_prefix(DESKTOP_TOOL_PREFIX)
            .ok_or_else(|| format!("Desktop tool name is not Pantheon owned: {name}"))?;
        if !DESKTOP_TOOLS.iter().any(|entry| entry.name == canonical) {
            return Err(format!("Desktop tool is not in the Pantheon contract: {name}"));
        }
        if !seen.insert(name) {
            return Err(format!("Duplicate desktop tool: {name}"));
        }
        if tool["inputSchema"]["type"] != "object" {
            return Err(format!("Desktop tool has no object input schema: {name}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str) -> serde_json::Value {
        json!({
            "type": "function",
            "name": name,
            "description": "test",
            "inputSchema": {"type": "object", "properties": {}, "required": []},
        })
    }

    #[test]
    fn contract_has_all_known_desktop_tools() {
        assert_eq!(DESKTOP_TOOLS.len(), 79);
        let names: BTreeSet<String> = DESKTOP_TOOLS.iter().map(|tool| full_name(*tool)).collect();
        assert_eq!(names.len(), DESKTOP_TOOLS.len(), "contract has duplicate names");
        assert!(names.contains("pantheon_capture_browser"));
        assert!(names.contains("pantheon_cloud_cli"));
    }

    #[test]
    fn catalog_validation_accepts_contract_tools() {
        let tools = vec![spec("pantheon_list_tasks"), spec("pantheon_open_file")];
        validate_catalog(&tools).expect("contract tools must validate");
    }

    #[test]
    fn catalog_validation_rejects_unowned_or_malformed_tools() {
        assert!(validate_catalog(&[spec("shell")]).is_err());
        assert!(validate_catalog(&[spec("pantheon_not_a_tool")]).is_err());
        assert!(validate_catalog(&[spec("pantheon_list_tasks"), spec("pantheon_list_tasks")]).is_err());
        let mut bad = spec("pantheon_list_tasks");
        bad["type"] = json!("namespace");
        assert!(validate_catalog(&[bad]).is_err());
        let mut bad = spec("pantheon_list_tasks");
        bad["inputSchema"] = json!({});
        assert!(validate_catalog(&[bad]).is_err());
    }

    #[test]
    fn domain_lookup_is_available_to_adapters() {
        assert_eq!(domain_of("pantheon_browser_click"), Some(DesktopToolDomain::Browser));
        assert_eq!(domain_of("pantheon_vm_exec"), Some(DesktopToolDomain::Device));
        assert_eq!(domain_of("shell"), None);
    }
}
