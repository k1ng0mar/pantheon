//! Pantheon's delegated agents, read from the tool calls the runtime emits.
//!
//! Pantheon spawns children through its own tools - `delegate_async` returns a
//! handle, and `subagent_read`/`subagent_wait`/`subagent_list` report on it - so
//! the child record is derived from those ordinary `tool_call` updates rather
//! than from a second, harness-specific side channel. Everything here is a view
//! over events the turn already carries; it never consumes a frame, so the main
//! transcript renders exactly as before.
use super::*;

const CHILD_LIMIT: usize = 64;
const RESPONSE_LIMIT: usize = 32 * 1024;
const CHILD_INPUT_LIMIT: usize = 2 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubagentActivity {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub prompt: String,
    pub status: String,
    pub response: String,
    pub tools: Vec<ToolActivity>,
    pub started_at: i64,
    pub updated_at: i64,
}

struct Child {
    activity: SubagentActivity,
}

#[derive(Default)]
pub(super) struct Subagents {
    children: HashMap<String, Child>,
    /// Handles reported by `subagent_list`, so a handle-only frame can be matched.
    handles: HashMap<String, String>,
    dirty: HashSet<String>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn active(status: &str) -> bool {
    matches!(status, "running" | "waiting")
}

impl Subagents {
    fn child(&mut self, id: &str) -> Option<&mut Child> {
        if id.is_empty() || id.len() > 256 {
            return None;
        }
        if !self.children.contains_key(id) {
            // Retire finished previews so later delegates stay visible; the
            // bound never evicts running work.
            if self.children.len() >= CHILD_LIMIT {
                let retired = self
                    .children
                    .iter()
                    .filter(|(_, child)| !active(&child.activity.status))
                    .min_by_key(|(_, child)| child.activity.updated_at)
                    .map(|(id, _)| id.clone())?;
                self.children.remove(&retired);
                self.dirty.remove(&retired);
            }
            let time = now();
            self.children.insert(
                id.into(),
                Child {
                    activity: SubagentActivity {
                        id: id.into(),
                        parent_id: None,
                        name: "Subagent".into(),
                        prompt: String::new(),
                        status: "running".into(),
                        response: String::new(),
                        tools: Vec::new(),
                        started_at: time,
                        updated_at: time,
                    },
                },
            );
            self.dirty.insert(id.into());
        }
        self.children.get_mut(id)
    }
    fn changed(&mut self, id: &str) {
        if let Some(child) = self.children.get_mut(id) {
            child.activity.updated_at = now();
            self.dirty.insert(id.into());
        }
    }
    pub(super) fn flush(&mut self, protocol: &Protocol) -> Vec<AgentEvent> {
        self.dirty
            .drain()
            .filter_map(|id| {
                let child = self.children.get(&id)?;
                let mut event = protocol.event("subagent", "", None);
                event.data = serde_json::to_value(&child.activity).ok();
                Some(event)
            })
            .collect()
    }
    /// Record delegated work from this turn's tool events. Child identity is the
    /// launching tool call's id, which is also what the transcript shows.
    pub(super) fn observe_tools(&mut self, events: &[AgentEvent], _config: &RunConfig) {
        for tool in events.iter().filter_map(|event| event.tool.as_ref()) {
            let base = tool.name.split(':').next().unwrap_or_default();
            let input: Value = serde_json::from_str(&tool.input).unwrap_or(Value::Null);
            let launching = matches!(base, "delegate_async" | "Agent" | "Task" | "agent");
            let reporting = matches!(
                base,
                "subagent_read" | "subagent_wait" | "subagent_status" | "subagent_list"
            );
            if !launching && !reporting && !self.children.contains_key(&tool.id) {
                continue;
            }
            if let Some(handle) = input["handle"].as_str().filter(|v| !v.is_empty()) {
                self.handles.insert(handle.to_string(), tool.id.clone());
            }
            let Some(child) = self.child(&tool.id) else {
                continue;
            };
            let before = child.activity.clone();
            if launching {
                if let Some(name) = ["agent", "name", "description", "subagent_type"]
                    .iter()
                    .find_map(|key| input[*key].as_str().filter(|value| !value.is_empty()))
                {
                    child.activity.name = limited(name, 256);
                }
                if let Some(prompt) = ["task", "prompt"]
                    .iter()
                    .find_map(|key| input[*key].as_str().filter(|value| !value.is_empty()))
                {
                    child.activity.prompt = limited(prompt, CHILD_INPUT_LIMIT);
                }
            }
            if child.activity.status != tool.status {
                child.activity.status = tool.status.clone();
            }
            if !tool.output.is_empty() && tool.status != "running" {
                let output = limited(&tool.output, RESPONSE_LIMIT);
                if child.activity.response != output {
                    child.activity.response = output;
                }
            }
            if before != child.activity {
                self.changed(&tool.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RunConfig {
        super::super::tests::config("pantheon")
    }

    fn tool(id: &str, name: &str, status: &str, input: Value, output: &str) -> AgentEvent {
        AgentEvent {
            kind: "tool".into(),
            tool: Some(ToolActivity {
                id: id.into(),
                name: name.into(),
                status: status.into(),
                input: input.to_string(),
                output: output.into(),
                write_paths: None,
            }),
            ..AgentEvent {
                session_id: String::new(),
                kind: String::new(),
                text: String::new(),
                request_id: None,
                data: None,
                tool: None,
            }
        }
    }

    #[test]
    fn a_delegated_child_carries_its_profile_and_objective() {
        let protocol = Protocol::new(config());
        let mut tracker = Subagents::default();
        tracker.observe_tools(
            &[tool(
                "call-1",
                "delegate_async",
                "running",
                serde_json::json!({"agent": "auditor", "task": "Review the parser"}),
                "",
            )],
            &config(),
        );
        let events = tracker.flush(&protocol);
        let activity: SubagentActivity =
            serde_json::from_value(events[0].data.clone().unwrap()).unwrap();
        assert_eq!(activity.id, "call-1");
        assert_eq!(activity.name, "auditor");
        assert_eq!(activity.prompt, "Review the parser");
        assert_eq!(activity.status, "running");
    }

    #[test]
    fn a_finished_child_reports_its_result_once() {
        let protocol = Protocol::new(config());
        let mut tracker = Subagents::default();
        tracker.observe_tools(
            &[tool(
                "call-2",
                "delegate_async",
                "running",
                serde_json::json!({"agent": "writer", "task": "Draft the notes"}),
                "",
            )],
            &config(),
        );
        tracker.flush(&protocol);
        tracker.observe_tools(
            &[tool(
                "call-2",
                "delegate_async",
                "completed",
                Value::Null,
                "Notes are ready",
            )],
            &config(),
        );
        let events = tracker.flush(&protocol);
        let activity: SubagentActivity =
            serde_json::from_value(events[0].data.clone().unwrap()).unwrap();
        assert_eq!(activity.status, "completed");
        assert_eq!(activity.response, "Notes are ready");
    }

    #[test]
    fn an_unrelated_tool_never_becomes_a_subagent() {
        let protocol = Protocol::new(config());
        let mut tracker = Subagents::default();
        tracker.observe_tools(
            &[tool("call-3", "Bash", "completed", Value::Null, "ok")],
            &config(),
        );
        assert!(tracker.flush(&protocol).is_empty());
    }
}
