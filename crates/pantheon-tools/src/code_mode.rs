//! Code Mode: a sandboxed JavaScript scratchpad with a narrow host bridge.
//!
//! The model writes JavaScript once and calls Pantheon tools from it:
//!
//! ```js
//! const hits = tools.call("grep", { pattern: "TODO", path: "src" });
//! const files = tools.call("list_dir", { path: "src" });
//! console.log(hits);
//! files;
//! ```
//!
//! The JS runtime has no filesystem, network, process, or module APIs of
//! its own. Its only escape hatch is `tools.call`, which goes through
//! [`ToolRegistry::execute_gated`] with the session's policy. A nested call
//! that needs operator approval is denied rather than silently granted:
//! Code Mode cannot park the turn for a human, so the safe default is to
//! tell the model to use the direct tool or ask first.
//!
//! The outer `execute` tool is registered as a host-mediated stub; the
//! runtime intercepts it and calls [`execute`] with the real registry and
//! policy. This keeps the registry immutable and avoids a self-referential
//! tool closure.

use crate::tools::{tool_err, ToolCallContext, ToolRegistry};
use pantheon_api::capability::Policy;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use rquickjs::{Context, Function, Runtime};
use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The bridge callbacks are installed as `'static` QuickJS functions, but
/// they only run synchronously inside [`run_js`], where these borrows are
/// valid. The thread-local keeps that lifetime contract explicit instead of
/// making the registry cloneable (its tool closures are not `Clone`).
#[derive(Clone)]
struct CodeCtx {
    registry: *const ToolRegistry,
    policy: *const Policy,
    call: Option<ToolCallContext>,
}

thread_local! {
    static CODE_CTX: RefCell<Option<CodeCtx>> = const { RefCell::new(None) };
}

struct CodeCtxGuard;

impl Drop for CodeCtxGuard {
    fn drop(&mut self) {
        CODE_CTX.with(|c| *c.borrow_mut() = None);
    }
}

fn install_ctx(
    registry: &ToolRegistry,
    policy: &Policy,
    call: Option<ToolCallContext>,
) -> CodeCtxGuard {
    CODE_CTX.with(|c| {
        *c.borrow_mut() = Some(CodeCtx {
            registry,
            policy,
            call,
        })
    });
    CodeCtxGuard
}

fn with_ctx<T>(f: impl FnOnce(&ToolRegistry, &Policy, &Option<ToolCallContext>) -> T) -> Option<T> {
    CODE_CTX.with(|c| {
        let guard = c.borrow();
        let ctx = guard.clone()?;
        // SAFETY: the guard installed in `run_js` keeps both referents
        // alive for the whole synchronous eval, and this thread is the only
        // reader of the thread-local.
        unsafe { Some(f(&*ctx.registry, &*ctx.policy, &ctx.call)) }
    })
}

pub const CODE_MODE_TOOL: &str = "execute";
const MAX_CODE_BYTES: usize = 128 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 30_000;

/// Register the model-visible `execute` stub. Execution happens in the
/// runtime's tool runner, which owns the registry and policy.
pub fn register_code_mode_stub(reg: &mut ToolRegistry) {
    reg.register(
        ToolSchema {
            name: CODE_MODE_TOOL.into(),
            description: "Run JavaScript in a sandbox and compose Pantheon tool calls. Use `tools.call(name, argsObject)` to invoke a tool; it returns that tool's text result (JSON where the tool returns JSON). `tools.names()` lists callable tools. `console.log(...)` output is returned with the result. No filesystem, network, process, or import APIs are available. A nested tool call is denied if it would need operator approval, so call approval-gated tools directly. The final expression is returned (top-level `return` is not supported; leave the value as the last expression)."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "JavaScript source. The final expression is returned."},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 30000}
                },
                "required": ["code"]
            }),
        },
        pantheon_api::capability::Capability::ShellExecute,
        |_args| {
            Err(tool_err(
                "TOOL_HOST_MEDIATED",
                Layer::Execution,
                false,
                "execute is host-mediated: call it through the agent loop, which supplies the tool registry and policy"
                    .into(),
                "check tool name and arguments",
            ))
        },
    );
}

/// Execute one Code Mode request. `args_json` is the tool-call arguments:
/// `{"code": "...", "timeout_ms": 5000}`.
pub fn execute(
    registry: &ToolRegistry,
    policy: &Policy,
    args_json: &str,
) -> Result<String, PantheonError> {
    execute_with_context(registry, policy, args_json, None)
}

/// Like [`execute`], but installs the outer call's durable context for
/// nested tool calls. The production adapter supplies this so a tool called
/// from Code Mode can link its side effects to the same step.
pub fn execute_with_context(
    registry: &ToolRegistry,
    policy: &Policy,
    args_json: &str,
    call: Option<ToolCallContext>,
) -> Result<String, PantheonError> {
    let v: serde_json::Value = serde_json::from_str(args_json).map_err(|e| {
        tool_err(
            "CODE_BAD_ARGS",
            Layer::Execution,
            false,
            format!("invalid execute args: {e}"),
            "pass {\"code\": \"...\"}",
        )
    })?;
    let code = v.get("code").and_then(|x| x.as_str()).ok_or_else(|| {
        tool_err(
            "CODE_BAD_ARGS",
            Layer::Execution,
            false,
            "missing 'code'".into(),
            "pass {\"code\": \"...\"}",
        )
    })?;
    if code.trim().is_empty() {
        return Err(tool_err(
            "CODE_EMPTY",
            Layer::Execution,
            false,
            "empty code".into(),
            "pass JavaScript to execute",
        ));
    }
    if code.len() > MAX_CODE_BYTES {
        return Err(tool_err(
            "CODE_TOO_LARGE",
            Layer::Execution,
            false,
            format!("code is {} bytes; limit is {MAX_CODE_BYTES}", code.len()),
            "split the work into smaller execute calls",
        ));
    }
    let timeout = Duration::from_millis(
        v.get("timeout_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(100, MAX_TIMEOUT_MS),
    );
    run_js(registry, policy, code, timeout, call)
}

fn run_js(
    registry: &ToolRegistry,
    policy: &Policy,
    code: &str,
    timeout: Duration,
    call: Option<ToolCallContext>,
) -> Result<String, PantheonError> {
    let rt = Runtime::new().map_err(|e| code_err("CODE_RUNTIME", e.to_string()))?;
    rt.set_memory_limit(64 * 1024 * 1024);
    rt.set_max_stack_size(512 * 1024);
    let deadline = Instant::now() + timeout;
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let ctx = Context::full(&rt).map_err(|e| code_err("CODE_CONTEXT", e.to_string()))?;
    let _ctx_guard = install_ctx(registry, policy, call);

    ctx.with(|ctx| {
        let host_call = Function::new(
            ctx.clone(),
            |name: String, args: String| -> String {
                if name == CODE_MODE_TOOL {
                    return serde_json::json!({
                        "ok": false,
                        "error": "recursive execute is not allowed"
                    })
                    .to_string();
                }
                let Some(result) = with_ctx(|registry, policy, call| match call {
                    Some(call) => registry.execute_gated_with_context(
                        policy,
                        &name,
                        &args,
                        call.clone(),
                    ),
                    None => registry.execute_gated(policy, &name, &args),
                }) else {
                    return serde_json::json!({
                        "ok": false,
                        "error": "Code Mode bridge is not installed"
                    })
                    .to_string();
                };
                match result {
                    Ok(out) => {
                        let out = truncate(out, MAX_RESULT_BYTES);
                        serde_json::json!({"ok": true, "result": out}).to_string()
                    }
                    Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
                }
            },
        )
        .map_err(|e| code_err("CODE_BRIDGE", e.to_string()))?;
        ctx.globals()
            .set("__pantheon_call", host_call)
            .map_err(|e| code_err("CODE_BRIDGE", e.to_string()))?;

        let host_names = Function::new(ctx.clone(), || -> String {
            let Some(names) = with_ctx(|registry, _, _| {
                let mut names: Vec<String> = registry
                    .names()
                    .into_iter()
                    .filter(|n| n != CODE_MODE_TOOL)
                    .collect();
                names.sort();
                names
            }) else {
                return "[]".to_string();
            };
            serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string())
        })
        .map_err(|e| code_err("CODE_BRIDGE", e.to_string()))?;
        ctx.globals()
            .set("__pantheon_names", host_names)
            .map_err(|e| code_err("CODE_BRIDGE", e.to_string()))?;

        let code_literal = serde_json::to_string(code)
            .map_err(|e| code_err("CODE_ARGS", e.to_string()))?;
        let source = format!(
            r#"
const __pantheon_logs = [];
const __pantheon_fmt = (x) => {{
  if (typeof x === "string") return x;
  try {{ return JSON.stringify(x); }} catch (_e) {{ return String(x); }}
}};
globalThis.console = {{
  log: (...args) => __pantheon_logs.push(args.map(__pantheon_fmt).join(" ")),
  error: (...args) => __pantheon_logs.push(args.map(__pantheon_fmt).join(" ")),
}};
globalThis.tools = {{
  call: (name, args) => {{
    const raw = __pantheon_call(String(name), typeof args === "string" ? args : JSON.stringify(args ?? {{}}));
    const env = JSON.parse(raw);
    if (!env.ok) throw new Error(env.error);
    return env.result;
  }},
  names: () => JSON.parse(__pantheon_names()),
}};
let __pantheon_out;
try {{
  const __pantheon_value = eval({code_literal});
  __pantheon_out = {{
    ok: true,
    result: __pantheon_value === undefined ? null : __pantheon_value,
  }};
}} catch (e) {{
  __pantheon_out = {{ ok: false, error: String(e) }};
}}
__pantheon_out.logs = __pantheon_logs;
globalThis.__pantheon_raw = JSON.stringify(__pantheon_out);
"#
        );
        ctx.eval::<(), _>(source)
            .map_err(|e| code_err("CODE_EVAL", format!("{e}")))?;
        let raw: String = ctx
            .eval("globalThis.__pantheon_raw")
            .map_err(|e| code_err("CODE_EVAL", format!("{e}")))?;
        let envelope: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            code_err(
                "CODE_RESULT",
                format!("Code Mode returned invalid JSON: {e}"),
            )
        })?;
        if !envelope.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let error = envelope
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("JavaScript execution failed");
            let logs = render_logs(&envelope);
            return Err(tool_err(
                "CODE_EVAL",
                Layer::Execution,
                false,
                if logs.is_empty() {
                    error.to_string()
                } else {
                    format!("{error}\n{logs}")
                },
                "fix the JavaScript and retry",
            ));
        }
        let mut out = match envelope.get("result") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Null) | None => String::new(),
            Some(other) => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        let logs = render_logs(&envelope);
        if !logs.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&logs);
        }
        if out.is_empty() {
            out.push_str("(no result)");
        }
        Ok(truncate(out, MAX_RESULT_BYTES))
    })
}

fn render_logs(envelope: &serde_json::Value) -> String {
    let Some(logs) = envelope.get("logs").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if logs.is_empty() {
        return String::new();
    }
    let mut out = String::from("logs:");
    for line in logs {
        out.push('\n');
        out.push_str(&truncate(
            line.as_str().unwrap_or_default().to_string(),
            4_000,
        ));
    }
    out
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("\n…[truncated]");
    s
}

fn code_err(code: &str, cause: String) -> PantheonError {
    tool_err(
        code,
        Layer::Execution,
        false,
        cause,
        "fix the JavaScript and retry",
    )
}

/// Help the model discover the bridge without exposing implementation
/// internals in the tool schema.
pub fn bridge_summary() -> &'static str {
    "tools.call(name, args), tools.names(), console.log(...)"
}

// The bridge summary is referenced by tests and can be used by a future
// system-prompt hint; keep it out of the hot path.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<()>>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use pantheon_api::capability::Capability;
    use pantheon_api::message::ToolSchema;

    fn registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(
            ToolSchema {
                name: "probe".into(),
                description: "echo args".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
            Capability::FilesystemRead,
            |args| Ok(format!("probe:{args}")),
        );
        reg
    }

    #[test]
    fn evaluates_and_returns_final_expression() {
        let out = execute(&registry(), &Policy::coder(), r#"{"code":"1 + 2"}"#).unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn calls_a_registered_tool_with_json_args() {
        let out = execute(
            &registry(),
            &Policy::coder(),
            r#"{"code":"tools.call('probe', {x: 7})"}"#,
        )
        .unwrap();
        assert!(out.contains("probe:{\"x\":7}"), "{out}");
    }

    #[test]
    fn console_logs_are_returned() {
        let out = execute(
            &registry(),
            &Policy::coder(),
            r#"{"code":"console.log('hello', {a: 1}); 5"}"#,
        )
        .unwrap();
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("logs:"), "{out}");
    }

    #[test]
    fn nested_denial_is_a_js_exception() {
        let mut reg = registry();
        reg.register(
            ToolSchema {
                name: "needs_approval".into(),
                description: "test".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
            Capability::GitPush,
            |_| Ok("should not run".into()),
        );
        let out = execute(
            &reg,
            &Policy::coder(),
            r#"{"code":"tools.call('needs_approval', {})"}"#,
        );
        assert!(out.is_err(), "{out:?}");
        assert!(out.unwrap_err().to_string().contains("TOOL_DENIED"));
    }

    #[test]
    fn timeout_stops_an_infinite_loop() {
        let out = execute(
            &registry(),
            &Policy::coder(),
            r#"{"code":"while (true) {}","timeout_ms":100}"#,
        );
        assert!(out.is_err(), "{out:?}");
    }
}
