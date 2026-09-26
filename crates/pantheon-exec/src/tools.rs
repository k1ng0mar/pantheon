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
    pub run: ToolFn,
    /// Extra capabilities this call needs on top of `capability`, decided
    /// from the arguments. A shell tool that sees `git push` returns
    /// `[GitPush]` here, so the loop gates the call on both.
    ///
    /// Without this, an argument-insensitive static capability made the
    /// `git push needs approval` policy rule unreachable: every push came
    /// through `shell`, whose only capability is ShellExecute.
    pub extra_capabilities: Option<ArgCapabilities>,
}

/// Derives extra required capabilities from one call's arguments.
pub type ArgCapabilities = Box<dyn Fn(&str) -> Vec<Capability> + Send + Sync>;

/// Boxed tool implementation: every tool is a sync string-in/string-out
/// closure behind the registry.
pub type ToolFn = Box<dyn Fn(&str) -> Result<String, PantheonError> + Send + Sync>;

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
        self.register_with(schema, capability, run, None);
    }

    /// Register a tool whose required capabilities depend on the arguments.
    pub fn register_with(
        &mut self,
        schema: ToolSchema,
        capability: Capability,
        run: impl Fn(&str) -> Result<String, PantheonError> + Send + Sync + 'static,
        extra_capabilities: Option<ArgCapabilities>,
    ) {
        self.tools.insert(
            schema.name.clone(),
            Tool {
                schema,
                capability,
                run: Box::new(run),
                extra_capabilities,
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

    /// Every capability one concrete call needs: the tool's static
    /// capability plus anything the arguments add.
    ///
    /// The loop gates on the full set, not the first capability, so a
    /// `git push` through `shell` is checked against GitPush (which the
    /// coder policy marks Approval) and not silently waved through as
    /// ShellExecute.
    pub fn required_capabilities(&self, name: &str, args: &str) -> Vec<Capability> {
        let Some(tool) = self.tools.get(name) else {
            return vec![Capability::Other(name.to_string())];
        };
        let mut caps = vec![tool.capability.clone()];
        if let Some(extra) = &tool.extra_capabilities {
            for c in extra(args) {
                if !caps.contains(&c) {
                    caps.push(c);
                }
            }
        }
        caps
    }
}

/// Parse a JSON args string; empty means empty object.
pub fn parse_args(args: &str) -> Result<serde_json::Value, PantheonError> {
    if args.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(args).map_err(|e| terr("TOOL_BAD_ARGS", format!("invalid JSON args: {e}")))
}
