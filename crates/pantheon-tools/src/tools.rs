//! tool registry: names -> schemas + capabilities, run behind the agent-loop
//! gate. runtime owns the tools; model just sees schemas + results.

use pantheon_api::capability::{Capability, Decision, Policy};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use std::collections::HashMap;

/// `PantheonError::new` with the evidence slot pinned empty. callers fill in
/// their own remediation; `retryable` means transient.
/// there used to be eight copy-pasted `berr` / `merr` / `name_err` / ...
/// constructors and they'd drifted apart.
pub(crate) fn tool_err(
    code: &str,
    layer: Layer,
    retryable: bool,
    cause: String,
    remediation: &str,
) -> PantheonError {
    PantheonError::new(code, layer, retryable, cause, remediation, "")
}

/// One registered tool: schema + capability + executor.
pub struct Tool {
    pub schema: ToolSchema,
    pub capability: Capability,
    pub run: ToolFn,
    /// extra caps this call needs beyond `capability`, picked from the args.
    /// a shell tool that sees `git push` returns `[GitPush]` so the loop gates both.
    ///
    /// without this the `git push needs approval` rule was unreachable: every
    /// push went through `shell`, whose only cap is ShellExecute.
    pub extra_capabilities: Option<ArgCapabilities>,
}

/// derives extra required capabilities from one call's args.
pub type ArgCapabilities = Box<dyn Fn(&str) -> Vec<Capability> + Send + Sync>;

/// Boxed tool impl: every tool is a sync string-in/string-out closure behind
/// the registry.
pub type ToolFn = Box<dyn Fn(&str) -> Result<String, PantheonError> + Send + Sync>;

/// identifies the tool call running on this thread, when the host installed one.
/// tools are plain `Fn(&str)` closures, so a call that has to key durable side
/// effects to its own call (the delegate tool links the child run it spawns to
/// the parent transcript step) reads it from here instead of threading a
/// parameter through every registration site. mirrors the TUI's `STREAM_RUN`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallContext {
    pub run_id: String,
    pub call_id: String,
}

std::thread_local! {
    static CURRENT_CALL: std::cell::RefCell<Option<ToolCallContext>> =
        const { std::cell::RefCell::new(None) };
}

/// The call context installed on this thread, if any.
pub fn current_call_context() -> Option<ToolCallContext> {
    CURRENT_CALL.with(|c| c.borrow().clone())
}

/// Registry of available tools for a run.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Tool>,
    /// loaded into this turn. auto starts with a small core set; model loads
    /// more via `load_tools`. always-available mode loads everything.
    loaded: std::collections::HashSet<String>,
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

    /// register a tool whose required caps depend on the args.
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

    /// remove a tool by name. nightly repair's host adapter uses this to
    /// disable a broken tool. returns true if one was actually removed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    /// Drop every tool whose capability the predicate rejects. Used by the
    /// surface profile: a `safe` session never sees a mutating tool at all,
    /// which is stronger than gating each call.
    pub fn retain(
        &mut self,
        keep: impl Fn(&str, &Capability, bool) -> bool,
    ) {
        self.tools.retain(|name, tool| {
            keep(
                name,
                &tool.capability,
                tool.extra_capabilities.is_some(),
            )
        });
        self.loaded.retain(|name| self.tools.contains_key(name));
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema.clone()).collect()
    }

    /// schemas for tools loaded this turn (auto mode). empty set = always-available.
    pub fn loaded_schemas(&self) -> Vec<ToolSchema> {
        if self.loaded.is_empty() {
            return self.schemas();
        }
        self.tools
            .values()
            .filter(|t| self.loaded.contains(&t.schema.name))
            .map(|t| t.schema.clone())
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// mark tools loaded for next turn. unknown names ignored. returns what actually loaded.
    pub fn load_tools(&mut self, names: &[String]) -> Vec<String> {
        let mut loaded = Vec::new();
        for name in names {
            if self.tools.contains_key(name) && self.loaded.insert(name.clone()) {
                loaded.push(name.clone());
            }
        }
        loaded
    }

    /// set the always-loaded core set (auto-mode start state).
    pub fn set_core_tools(&mut self, names: &[String]) {
        self.loaded = names
            .iter()
            .filter(|&n| self.tools.contains_key(n))
            .cloned()
            .collect();
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.is_empty() || self.loaded.contains(name)
    }

    /// execute by name. unknown tool = structured error, never a panic.
    pub fn execute(&self, name: &str, args: &str) -> Result<String, PantheonError> {
        let tool = self.tools.get(name).ok_or_else(|| {
            tool_err(
                "TOOL_UNKNOWN",
                Layer::Execution,
                false,
                format!("no tool named '{name}'"),
                "check tool name and arguments",
            )
        })?;
        // Repair the common JSON malformations and coerce values to the
        // schema before the tool sees them. A malformed call should cost one
        // rewrite, not a failed turn.
        let (args, note) = crate::arg_repair::repair_args(args, &tool.schema.parameters);
        if note.fired() {
            pantheon_api::logging::warn(
                "tool",
                format!("{name}: repaired arguments ({})", note.summary()),
            );
        }
        (tool.run)(&args)
    }

    /// execute with a [`ToolCallContext`] installed on this thread for the call.
    /// restores the previous context on return so nested executions don't leak
    /// their identity into the caller.
    pub fn execute_with_context(
        &self,
        name: &str,
        args: &str,
        ctx: ToolCallContext,
    ) -> Result<String, PantheonError> {
        let previous = CURRENT_CALL.with(|c| c.replace(Some(ctx)));
        let result = self.execute(name, args);
        CURRENT_CALL.with(|c| *c.borrow_mut() = previous);
        result
    }

    /// capability the tool requires (for the loop's gate).
    pub fn capability_of(&self, name: &str) -> Option<Capability> {
        self.tools.get(name).map(|t| t.capability.clone())
    }

    /// every cap one concrete call needs: static cap + anything the args add.
    ///
    /// loop gates on the FULL set, not the first cap. that's why a `git push`
    /// through `shell` gets checked against GitPush (Approval in coder policy)
    /// instead of being waved through as plain ShellExecute.
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

    /// execute by name with the capability gate applied.
    ///
    /// `execute` above does no gating on purpose - it just resolves a name and
    /// calls the closure. fine for a caller that already gated (the agent loop,
    /// which needs the approval/diagnostic flow), but everyone else who used it
    /// ran the tool ungated: the CLI's direct `reg.execute("vault_*", ..)` calls
    /// and the plugin tool closures in `supervisor.rs` both did.
    ///
    /// gate lives here so new callers get enforcement by default. denials are
    /// structured errors naming the tool + missing cap, not a silent refusal.
    pub fn execute_gated(
        &self,
        policy: &Policy,
        name: &str,
        args: &str,
    ) -> Result<String, PantheonError> {
        // resolve the name first. `required_capabilities` maps an unknown name
        // to `Other(name)`, so gating first would report a cap denial for a tool
        // that doesn't exist - hiding the one thing the caller can fix (a typo).
        if !self.tools.contains_key(name) {
            return Err(tool_err(
                "TOOL_UNKNOWN",
                Layer::Execution,
                false,
                format!("no tool named '{name}'"),
                "check tool name and arguments",
            ));
        }
        let required = self.required_capabilities(name, args);
        for cap in &required {
            match policy.check(cap) {
                Decision::Allow => {}
                // approval isn't a denial, and this choke point has no run id or
                // scope to fill a grant command with. the old message told direct
                // callers (CLI vault, nightly probes, Code Mode) to run
                // `pantheon run --taskID <run> --grant <scope>` - which names
                // nothing real in their context. say what's actually needed.
                // stays TOOL_DENIED: callers key on it (UI maps it to
                // UI_TOOL_DENIED), the cause text just says which verdict fired.
                Decision::Approval => {
                    return Err(tool_err("TOOL_DENIED", Layer::Execution, false, format!(
                            "tool '{name}' needs capability {cap:?}, which requires operator approval; \
                             run it through a session where an operator can approve this call, or widen the policy preset"
                        ), "check tool name and arguments"))
                }
                Decision::Deny => {
                    return Err(tool_err("TOOL_DENIED", Layer::Execution, false, format!(
                            "tool '{name}' needs capability {cap:?}, which the policy denies; \
                             widen the policy preset to allow it"
                        ), "check tool name and arguments"))
                }
            }
        }
        self.execute(name, args)
    }

    /// [`execute_gated`] with a call context installed. Code Mode uses this so
    /// nested delegate/tool calls can key durable side effects to the outer
    /// step. previous context restored on return.
    pub fn execute_gated_with_context(
        &self,
        policy: &Policy,
        name: &str,
        args: &str,
        ctx: ToolCallContext,
    ) -> Result<String, PantheonError> {
        let previous = CURRENT_CALL.with(|c| c.replace(Some(ctx)));
        let result = self.execute_gated(policy, name, args);
        CURRENT_CALL.with(|c| *c.borrow_mut() = previous);
        result
    }
}

/// parse a JSON args string; empty = empty object.
pub fn parse_args(args: &str) -> Result<serde_json::Value, PantheonError> {
    if args.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(args).map_err(|e| {
        tool_err(
            "TOOL_BAD_ARGS",
            Layer::Execution,
            false,
            format!("invalid JSON args: {e}"),
            "check tool name and arguments",
        )
    })
}

/// pull a required string arg out of parsed tool args.
///
/// this was copy-pasted in five places (`builtins`, `session_search_tools`,
/// `browser/tools`, `websearch/tools`, `exec/skills`) with the same
/// `TOOL_BAD_ARGS` shape + message. public because every tool module needs it.
pub fn arg_str(v: &serde_json::Value, key: &str) -> Result<String, PantheonError> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            tool_err(
                "TOOL_BAD_ARGS",
                Layer::Execution,
                false,
                format!("missing string arg '{key}'"),
                "check tool arguments",
            )
        })
}

#[cfg(test)]
mod arg_str_tests {
    use super::*;

    #[test]
    fn present_string_is_returned() {
        let v = serde_json::json!({"path": "/tmp/x", "n": 3});
        assert_eq!(arg_str(&v, "path").unwrap(), "/tmp/x");
    }

    #[test]
    fn missing_key_is_a_structured_bad_args_error() {
        let v = serde_json::json!({"other": 1});
        let err = arg_str(&v, "path").unwrap_err();
        assert_eq!(err.code, "TOOL_BAD_ARGS");
    }

    #[test]
    fn non_string_value_is_rejected_not_coerced() {
        // A number or object must not be silently stringified into a path.
        let v = serde_json::json!({"path": 42});
        let err = arg_str(&v, "path").unwrap_err();
        assert_eq!(err.code, "TOOL_BAD_ARGS");
    }

    #[test]
    fn empty_string_is_accepted_here_and_filtered_by_callers_that_need_it() {
        // `arg_str` reports presence, not usefulness; `exec/skills` filters
        // empty separately because a skill name must be non-empty.
        let v = serde_json::json!({"name": ""});
        assert_eq!(arg_str(&v, "name").unwrap(), "");
    }
}

#[cfg(test)]
mod call_context_tests {
    use super::*;
    use pantheon_api::capability::Capability;
    use pantheon_api::message::ToolSchema;

    fn probe_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(
            ToolSchema {
                name: "probe".into(),
                description: "reports the current call context".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            Capability::Other("probe".into()),
            |_args| match current_call_context() {
                Some(c) => Ok(format!("{}:{}", c.run_id, c.call_id)),
                None => Ok("none".to_string()),
            },
        );
        reg
    }

    #[test]
    fn execute_gated_names_the_verdict_without_a_fake_grant_command() {
        // BinaryTrace tool: coder policy parks it (Approval), default denies it.
        // neither error may name `pantheon run --taskID <run> --grant <scope>`:
        // execute_gated has no run/scope, so that "remediation" was placeholders.
        let mut reg = ToolRegistry::new();
        reg.register(
            ToolSchema {
                name: "trace".into(),
                description: "binary trace".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            Capability::BinaryTrace,
            |_| Ok("ran".to_string()),
        );
        let approval = reg
            .execute_gated(&Policy::coder(), "trace", "")
            .expect_err("coder policy parks binary trace");
        assert_eq!(approval.code, "TOOL_DENIED");
        assert!(
            approval.cause.contains("requires operator approval"),
            "approval must not read as a denial: {}",
            approval.cause
        );
        assert!(
            !approval.cause.contains("--taskID"),
            "no fake grant command: {}",
            approval.cause
        );
        let denied = reg
            .execute_gated(&Policy::default(), "trace", "")
            .expect_err("default policy denies binary trace");
        assert_eq!(denied.code, "TOOL_DENIED");
        assert!(
            denied.cause.contains("policy denies"),
            "denial must say denied: {}",
            denied.cause
        );
        assert!(
            !denied.cause.contains("--taskID"),
            "no fake grant command: {}",
            denied.cause
        );
    }

    #[test]
    fn loaded_schemas_start_empty_and_load_by_name() {
        let mut reg = probe_registry();
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "extra".into(),
                description: "x".into(),
                parameters: serde_json::json!({}),
            },
            pantheon_api::capability::Capability::FilesystemRead,
            |_| Ok("x".into()),
        );
        // empty loaded set = always available.
        assert!(!reg.loaded_schemas().is_empty());
        reg.set_core_tools(&["probe".into()]);
        assert_eq!(reg.loaded_schemas().len(), 1);
        let loaded = reg.load_tools(&["extra".into(), "missing".into()]);
        assert_eq!(loaded, vec!["extra".to_string()]);
        assert_eq!(reg.loaded_schemas().len(), 2);
        assert!(reg.is_loaded("extra"));
        assert!(!reg.is_loaded("missing"));
    }

    #[test]
    fn context_is_installed_for_one_call_and_restored_after() {
        let reg = probe_registry();
        // no context outside an installed execution.
        assert_eq!(current_call_context(), None);
        assert_eq!(reg.execute("probe", "").unwrap(), "none");
        let ctx = ToolCallContext {
            run_id: "run-1".into(),
            call_id: "call_3".into(),
        };
        assert_eq!(
            reg.execute_with_context("probe", "", ctx).unwrap(),
            "run-1:call_3"
        );
        // restored after: later plain calls see no context.
        assert_eq!(current_call_context(), None);
        assert_eq!(reg.execute("probe", "").unwrap(), "none");
    }
}
