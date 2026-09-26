//! Canonical-event -> hook bridge.
//!
//! `Supervisor::emit` already fans out to registered observers *after* the
//! durable ledger write, with exactly the fail-open semantics a hook needs
//! ("the ledger is the contract, observers are best-effort views"). This
//! module is a pure function from that stream to hook fires, so the runtime
//! needs one registration point instead of sprinkling hook calls through the
//! agent loop and the provider plane.
//!
//! Keeping it here (not in the runtime) also keeps the dependency direction
//! clean: `pantheon-extensions` already depends on `pantheon-core`, and the
//! runtime depends on extensions — never the reverse.

use crate::hooks::Hook;
use pantheon_core::events::Event;
use std::collections::HashMap;
use std::sync::Arc;

/// One hook fire derived from an event.
pub struct HookFire {
    pub hook: Hook,
    /// Flat string payload. The subprocess runners exchange JSON, and a flat
    /// map keeps both runners' input shape identical.
    pub extra: HashMap<String, String>,
}

/// Whether per-delta streaming hooks are enabled. Off by default: the runner
/// spawns a process per fire and `ModelDelta` arrives per token, so leaving
/// this on would be a self-inflicted stall. Hermes reaches the same conclusion
/// ("off the token path").
fn stream_deltas_enabled() -> bool {
    matches!(
        std::env::var("PANTHEON_HOOK_STREAM_DELTA").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Map one canonical event to the hook it should fire, if any.
///
/// Returns `None` for events with no hook (bookkeeping, approval, decision,
/// context-trim, …). Observers only: gate and transform hooks are *not*
/// driven from here because they must run inline and their return value
/// changes control flow — `pre_tool_call` and `transform_tool_result` are
/// fired from the tool-execution path in `session.rs` instead.
pub fn dispatch(ev: &Event) -> Option<HookFire> {
    use Event::*;
    let (hook, extra) = match ev {
        RunStarted { run_id } => (Hook::OnSessionStart, kv(&[("run_id", run_id)])),
        RunCompleted { run_id } | RunFailed { run_id, .. } | RunCanceled { run_id, .. } => {
            (Hook::OnSessionEnd, kv(&[("run_id", run_id)]))
        }
        ModelRequested { run_id, model } => (
            Hook::PreApiRequest,
            kv(&[("run_id", run_id), ("model", model)]),
        ),
        ModelCompleted { run_id } => {
            // A completed model turn is also the close of its stream. Hermes
            // keeps these distinct because a provider can emit deltas across
            // several `ModelRequested`s in one turn; here the request is the
            // unit, so one event closes exactly one stream.
            (Hook::PostApiRequest, kv(&[("run_id", run_id)]))
        }
        ModelDelta { run_id, delta } => {
            if !stream_deltas_enabled() {
                return None;
            }
            (
                Hook::OnStreamDelta,
                kv(&[("run_id", run_id), ("delta", delta)]),
            )
        }
        AgentSpawned { run_id, agent } => (
            Hook::SubagentStart,
            kv(&[("run_id", run_id), ("agent", agent)]),
        ),
        AgentCompleted { run_id, agent } => (
            Hook::SubagentStop,
            kv(&[("run_id", run_id), ("agent", agent)]),
        ),
        // Context compaction ran (rows trimmed or summarized). OMP emits
        // this as a start/end pair; Pantheon fires once, after the fact —
        // see `Hook::OnCompaction` for the documented collapse.
        ContextTrimmed { run_id, .. } => (Hook::OnCompaction, kv(&[("run_id", run_id)])),
        ContextCompressed { run_id, .. } => (Hook::OnCompaction, kv(&[("run_id", run_id)])),
        // `ToolStarted` is deliberately NOT mapped: its natural hook
        // (`pre_tool_call`) is a gate whose return value must change control
        // flow, and an observer's return value is ignored. It is fired
        // inline in `session.rs` before the tool executes. Likewise
        // `transform_tool_result` needs the output string in hand, which
        // `ToolOutput` does not carry, so it fires inline too.
        ToolCompleted {
            run_id,
            call_id,
            tool,
            ..
        } => (
            Hook::PostToolCall,
            kv(&[("run_id", run_id), ("call_id", call_id), ("tool", tool)]),
        ),
        _ => return None,
    };
    Some(HookFire { hook, extra })
}

/// The stream-open/close pair, which cannot be inferred from a single event
/// because `ModelRequested` also backs `pre_api_request`. The runtime calls
/// this alongside [`dispatch`] so a model turn notifies both facts.
pub fn dispatch_stream_edges(ev: &Event) -> Option<HookFire> {
    match ev {
        Event::ModelRequested { run_id, model } => Some(HookFire {
            hook: Hook::OnStreamStart,
            extra: kv(&[("run_id", run_id), ("model", model)]),
        }),
        Event::ModelCompleted { run_id } => Some(HookFire {
            hook: Hook::OnStreamEnd,
            extra: kv(&[("run_id", run_id)]),
        }),
        _ => None,
    }
}

fn kv(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Fire hooks off the caller's thread.
///
/// The supervisor's observer contract is explicit that observers run
/// *synchronously on the emit path* and must not block. A hook fire spawns a
/// subprocess (python3/node, up to the 10s runner timeout), so doing that
/// inline would stall the agent loop on every `ToolCompleted`.
///
/// [`HookDispatcher`] therefore queues each event's hooks onto a
/// short-lived worker thread and returns immediately. Ordering is preserved
/// within one event's hook pair, but not across events: observers are
/// notifications, not a ledger. If nobody binds a hook, the worker finds no
/// plugin and exits.
///
/// Terminal-event caveat: `on_session_end` is queued, not synchronous, so a
/// process that exits immediately after a run may not flush it. Consumers
/// that must not lose the event (GalaxyMem consolidation) should treat it as
/// best-effort and reconcile on next start, or await [`HookDispatcher::drain`]
/// at teardown.
pub struct HookDispatcher {
    mgr: std::sync::Arc<crate::manager::ExtensionManager>,
    inflight: Arc<std::sync::atomic::AtomicUsize>,
}

impl HookDispatcher {
    pub fn new(mgr: std::sync::Arc<crate::manager::ExtensionManager>) -> Self {
        Self {
            mgr,
            inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
    /// Queue the hooks implied by `ev`. Returns immediately.
    pub fn fire(&self, ev: &Event) {
        let fires: Vec<HookFire> = [dispatch(ev), dispatch_stream_edges(ev)]
            .into_iter()
            .flatten()
            .collect();
        if fires.is_empty() {
            return;
        }
        self.inflight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mgr = std::sync::Arc::clone(&self.mgr);
        let inflight = Arc::clone(&self.inflight);
        std::thread::spawn(move || {
            for f in fires {
                let session = f.extra.get("run_id").cloned().unwrap_or_default();
                mgr.notify(f.hook, &session, "runtime", f.extra);
            }
            inflight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    /// Block until queued fires have run. Bounded so a wedged plugin (the
    /// runner's own timeout) cannot hang the caller forever.
    pub fn drain(&self, budget: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        while self.inflight.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        true
    }
}

#[cfg(test)]
#[path = "event_bridge_tests.rs"]
mod tests;
