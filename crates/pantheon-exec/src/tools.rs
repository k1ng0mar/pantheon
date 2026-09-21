//! Tool registry: named tools with schemas + capability mapping, executed
//! behind the agent-loop gate. Tools are runtime-owned; the model only sees
//! schemas and gets results.

use pantheon_core::capability::Capability;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::ToolSchema;
use std::collections::HashMap;

fn terr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check tool name and arguments",
        "",
    )
}

/// One registered tool: schema + capability + executor.
pub struct Tool {
    pub schema: ToolSchema,
    pub capability: Capability,
    pub run: Box<dyn Fn(&str) -> Result<String, PantheonError> + Send + Sync>,
}

/// Registry of available tools for a run.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Tool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        schema: ToolSchema,
        capability: Capability,
        run: impl Fn(&str) -> Result<String, PantheonError> + Send + Sync + 'static,
    ) {
        self.tools.insert(
            schema.name.clone(),
            Tool {
                schema,
                capability,
                run: Box::new(run),
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema.clone()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Execute by name. Unknown tool is a structured error, never a panic.
    pub fn execute(&self, name: &str, args: &str) -> Result<String, PantheonError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| terr("TOOL_UNKNOWN", format!("no tool named '{name}'")))?;
        (tool.run)(args)
    }

    /// Capability a tool requires (for the loop's gate).
    pub fn capability_of(&self, name: &str) -> Option<Capability> {
        self.tools.get(name).map(|t| t.capability.clone())
    }
}

/// Parse a JSON args string; empty means empty object.
pub fn parse_args(args: &str) -> Result<serde_json::Value, PantheonError> {
    if args.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(args).map_err(|e| terr("TOOL_BAD_ARGS", format!("invalid JSON args: {e}")))
}
