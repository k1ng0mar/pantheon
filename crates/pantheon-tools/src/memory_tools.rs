//! Model-facing memory tools: five tools the agent can call to recall,
//! list, propose, forget, and confirm records. All writes go through
//! `propose -> policy -> provenance -> validation` and emit ledger events.
//! Reads respect `MemoryRead`. Writes require `MemoryWrite` and may
//! require approval per the runtime policy. Confirming (trust-tier
//! promotion) requires the separate `MemoryConfirm` capability, which
//! the default policies mark as needing human approval.
//!
//! The tools never touch the SQLite layer directly; they call the same
//! `MemoryBackend` capability gate the CLI does. A backend swap (native
//! store, GalaxyMem, Mnemosyne, Honcho, Hindsight) only changes the
//! `MemoryStore` implementation passed in here.
use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_memory::{
    confirm_via, recall_via, write_via, LayerKind, MemoryBackend, Proposal, Provenance,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Where tool events go. The runtime session implements this with its
/// ledger supervisor; tests pass a thread-safe Vec.
pub trait MemoryToolSink: Send + Sync {
    fn record(&self, event: MemoryToolEvent);
}

/// Concrete sink used by tests: collects events into a shared Vec.
#[derive(Clone, Default)]
pub struct VecMemorySink {
    pub events: std::sync::Arc<std::sync::Mutex<Vec<MemoryToolEvent>>>,
}

impl VecMemorySink {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn snapshot(&self) -> Vec<MemoryToolEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl MemoryToolSink for VecMemorySink {
    fn record(&self, event: MemoryToolEvent) {
        if let Ok(mut g) = self.events.lock() {
            g.push(event);
        }
    }
}

/// What the tools tell the outside world. Distinct from `Event` so
/// memory tools stay decoupled from the agent event enum, but the
/// runtime session projects these into the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryToolEvent {
    Recalled {
        query: String,
        hits: usize,
    },
    Listed {
        namespace: String,
        rows: usize,
    },
    Proposed {
        layer: LayerKind,
        namespace: String,
        key: String,
        value_len: usize,
        origin: String,
    },
    Written {
        layer: LayerKind,
        namespace: String,
        key: String,
        backend: String,
    },
    Forgotten {
        layer: LayerKind,
        namespace: String,
        key: String,
    },
    Denied {
        code: String,
        cause: String,
    },
}

impl MemoryToolSink for Arc<std::sync::Mutex<Vec<MemoryToolEvent>>> {
    fn record(&self, event: MemoryToolEvent) {
        if let Ok(mut g) = self.lock() {
            g.push(event);
        }
    }
}

/// Options for `register_memory_tools`. The store and policy are shared
/// across the five tools. `max_bytes` caps proposal sizes the same way
/// the CLI does.
#[derive(Clone)]
pub struct MemoryToolOptions {
    /// Active backend (native store or any registered plugin backend:
    /// GalaxyMem, Mnemosyne, Honcho, Hindsight, OpenViking, http bridge).
    /// Gating runs through `recall_via`/`write_via`/`confirm_via` before
    /// the backend sees anything.
    pub store: Arc<dyn MemoryBackend>,
    pub policy: Arc<pantheon_api::capability::Policy>,
    pub namespace: String,
    pub max_bytes: usize,
    pub sink: Arc<dyn MemoryToolSink>,
    pub backend_label: String,
}

impl MemoryToolOptions {
    pub fn now(&self) -> Provenance {
        Provenance {
            source: self.backend_label.clone(),
            origin: "model".into(),
            // Model-authored proposals are untrusted; propose_write clamps
            // them anyway, this makes the intent explicit at the source.
            trust: pantheon_api::provenance::TrustTier::Untrusted,
            recorded_at_ms: now_ms(),
        }
    }
}

fn layer_str_to_kind(s: &str) -> Option<LayerKind> {
    match s {
        "global" => Some(LayerKind::Global),
        "agent" => Some(LayerKind::Agent),
        "project" => Some(LayerKind::Project),
        "task" | "task_session" | "session" => Some(LayerKind::TaskSession),
        _ => None,
    }
}

/// Resolve a tool-supplied namespace against the session's own.
///
/// The namespace is the memory isolation boundary: a session confined to
/// `proj_a` must not be able to read, write, or forget records belonging to
/// `proj_b` just by naming it in the tool arguments. The model chooses the
/// argument, so the harness has to hold the boundary, not the model.
///
/// An absent or empty argument means "this session's namespace", which keeps
/// the common case working. An explicit argument that is anything else is
/// refused with a message naming the boundary, rather than silently
/// redirected, so a caller that expected cross-namespace access finds out.
/// Resolve a tool-supplied namespace against the session's own.
///
/// Public because this *is* the memory isolation boundary, and the agent
/// runtime's tests need to assert the refusal directly rather than through
/// a tool round trip. The behaviour is unchanged; only the visibility is.
pub fn resolve_namespace(
    requested: Option<&str>,
    session_ns: &str,
) -> Result<String, PantheonError> {
    match requested.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(session_ns.to_string()),
        Some(ns) if ns == session_ns => Ok(ns.to_string()),
        Some(other) => Err(crate::tools::tool_err(
            "MEM_NAMESPACE_DENIED",
            Layer::Execution,
            false,
            format!(
                "this session may only use namespace '{session_ns}'; \
                 '{other}' belongs to another session"
            ),
            "check tool arguments and policy",
        )),
    }
}

pub fn register_memory_tools(reg: &mut ToolRegistry, opts: MemoryToolOptions) {
    let recall_opts = opts.clone();
    reg.register(
        ToolSchema {
            name: "memory_recall".into(),
            description:
                "Search native memory across layers. Returns the matching records as key/value lines."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Free-text search query."},
                    "limit": {"type": "integer", "description": "Max records to return (default 8)."}
                },
                "required": ["query"]
            }),
        },
        Capability::MemoryRead,
        move |args| {
            let v = parse_args(args)?;
            let query = v
                .get("query")
                .and_then(|x| x.as_str())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'query'".into(), "check tool arguments and policy"))?
                .to_string();
            let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(8) as usize;
            let layers = [
                LayerKind::Project,
                LayerKind::Agent,
                LayerKind::Global,
            ];
            // Recall is scoped to this agent's own namespace. Recall took
            // no namespace at all before, so `memory_recall` returned
            // every agent's records from a shared `memory.db` — the write
            // path was already scoped, which made the read side the leak.
            let namespaces = [recall_opts.namespace.as_str()];
            let hits = match recall_via(
                recall_opts.store.as_ref(),
                &recall_opts.policy,
                &namespaces,
                &layers,
                &query,
                limit,
            ) {
                Ok(h) => h,
                Err(e) => {
                    recall_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    return Err(e);
                }
            };
            recall_opts.sink.record(MemoryToolEvent::Recalled {
                query: query.clone(),
                hits: hits.len(),
            });
            let mut out = String::new();
            for h in &hits {
                // Trust framing: recalled records are context, not
                // instructions. Untrusted-sourced records are flagged so
                // the model weighs them accordingly.
                let trust_tag = match h.record.provenance.trust {
                    pantheon_api::provenance::TrustTier::Untrusted => {
                        format!(" [untrusted: {}]", h.record.provenance.source)
                    }
                    t => format!(" [trust:{}]", t.as_str()),
                };
                out.push_str(&format!(
                    "- [{:?}] {} = {}{}\n",
                    h.record.layer, h.record.key, h.record.value, trust_tag
                ));
            }
            if out.is_empty() {
                out.push_str("(no matches)\n");
            }
            Ok(out)
        },
    );

    let list_opts = opts.clone();
    reg.register(
        ToolSchema {
            name: "memory_list".into(),
            description: "List Agent-layer records in the current namespace. Output is bounded: at most `limit` records (default 50, max 500); a `truncated: true` marker says when more exist.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "Max records to return (default 50, max 500)."}
                }
            }),
        },
        Capability::MemoryRead,
        move |args| {
            let v = parse_args(args)?;
            // Bound the output: an unbounded list dumps the whole agent
            // memory into the model's context. Clamp to [1, 500].
            let limit = v
                .get("limit")
                .and_then(|x| x.as_u64())
                .map(|n| n as usize)
                .unwrap_or(50)
                .clamp(1, 500);
            let rows = match list_opts.store.list_agent(&list_opts.namespace) {
                Ok(r) => r,
                Err(e) => {
                    list_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    return Err(e);
                }
            };
            let truncated = rows.len() > limit;
            let shown = rows.len().min(limit);
            list_opts.sink.record(MemoryToolEvent::Listed {
                namespace: list_opts.namespace.clone(),
                rows: shown,
            });
            let mut out = String::new();
            for (k, val) in rows.iter().take(limit) {
                out.push_str(&format!("- {k} = {val}\n"));
            }
            if out.is_empty() {
                out.push_str("(no agent records)\n");
            } else if truncated {
                out.push_str(&format!(
                    "truncated: true (showing first {limit} of {} records)\n",
                    rows.len()
                ));
            }
            Ok(out)
        },
    );

    let propose_opts = opts.clone();
    reg.register(
        ToolSchema {
            name: "memory_propose".into(),
            description:
                "Propose a memory write. Goes through propose -> policy -> provenance -> validation."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "value": {"type": "string"},
                    "layer": {"type": "string", "enum": ["global", "agent", "project", "task"]},
                    "namespace": {"type": "string", "description": "Defaults to session namespace."}
                },
                "required": ["key", "value"]
            }),
        },
        Capability::MemoryWrite,
        move |args| {
            let v = parse_args(args)?;
            let key = v
                .get("key")
                .and_then(|x| x.as_str())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'key'".into(), "check tool arguments and policy"))?
                .to_string();
            let value = v
                .get("value")
                .and_then(|x| x.as_str())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'value'".into(), "check tool arguments and policy"))?
                .to_string();
            let layer = v
                .get("layer")
                .and_then(|x| x.as_str())
                .and_then(layer_str_to_kind)
                .unwrap_or(LayerKind::Agent);
            let namespace = resolve_namespace(
                v.get("namespace").and_then(|x| x.as_str()),
                &propose_opts.namespace,
            )?;
            // Origin is harness-assigned ("model"), not model-claimed: a
            // proposal cannot launder untrusted material into trusted
            // memory by passing origin:"user". propose_write clamps the
            // tier for non-user origins regardless.
            let origin = "model".to_string();

            let mut prov = propose_opts.now();
            prov.origin = origin.clone();

            let proposal = Proposal {
                layer,
                namespace: namespace.clone(),
                key: key.clone(),
                value: value.clone(),
                provenance: prov,
            };

            propose_opts.sink.record(MemoryToolEvent::Proposed {
                layer,
                namespace: namespace.clone(),
                key: key.clone(),
                value_len: value.len(),
                origin: origin.clone(),
            });

            match write_via(
                propose_opts.store.as_ref(),
                &propose_opts.policy,
                proposal,
                propose_opts.max_bytes,
            ) {
                Ok(rec) => {
                    let key = rec.key.clone();
                    propose_opts.sink.record(MemoryToolEvent::Written {
                        layer: rec.layer,
                        namespace: rec.namespace,
                        key: key.clone(),
                        backend: propose_opts.backend_label.clone(),
                    });
                    Ok(format!("stored {} = {}", key, rec.value))
                }
                Err(e) => {
                    propose_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    Err(e)
                }
            }
        },
    );

    let forget_opts = opts.clone();
    reg.register(
        ToolSchema {
            name: "memory_forget".into(),
            description: "Remove a memory record. Goes through the same gate as propose.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "layer": {"type": "string", "enum": ["global", "agent", "project", "task"]},
                    "namespace": {"type": "string"}
                },
                "required": ["key"]
            }),
        },
        Capability::MemoryWrite,
        move |args| {
            let v = parse_args(args)?;
            let key = v
                .get("key")
                .and_then(|x| x.as_str())
                .ok_or_else(|| {
                    crate::tools::tool_err(
                        "TOOL_BAD_ARGS",
                        Layer::Execution,
                        false,
                        "missing 'key'".into(),
                        "check tool arguments and policy",
                    )
                })?
                .to_string();
            let layer = v
                .get("layer")
                .and_then(|x| x.as_str())
                .and_then(layer_str_to_kind)
                .unwrap_or(LayerKind::Agent);
            let namespace = resolve_namespace(
                v.get("namespace").and_then(|x| x.as_str()),
                &forget_opts.namespace,
            )?;

            forget_opts.sink.record(MemoryToolEvent::Forgotten {
                layer,
                namespace: namespace.clone(),
                key: key.clone(),
            });

            match forget_opts.store.forget(layer, &namespace, &key) {
                Ok(true) => Ok(format!("forgot {key}")),
                Ok(false) => Err(crate::tools::tool_err(
                    "MEM_NOT_FOUND",
                    Layer::Execution,
                    false,
                    format!("no record for {key} in {namespace}"),
                    "check tool arguments and policy",
                )),
                Err(e) => {
                    forget_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    Err(e)
                }
            }
        },
    );

    // memory_confirm: the user-promotion path. Promotes an Untrusted
    // record to Memory tier. It cannot exceed Memory tier: System and
    // User are reserved for harness and human authors.
    //
    // Gated on Capability::MemoryConfirm, which the default policies
    // mark as requiring approval — NOT on MemoryWrite. The old gate
    // (MemoryWrite, Allow) plus the "call only after the user vouched"
    // description was honor-system: a prompt-injected model could
    // propose then confirm its own poisoned record into the trusted
    // tier. Now the run loop parks the call for a human first.
    let confirm_opts = opts.clone();
    reg.register(
        ToolSchema {
            name: "memory_confirm".into(),
            description: "Mark an existing memory record as user-confirmed. Requires human approval under the default policy: the run parks until a user vouches for the record's content. Unconfirmed records stay untrusted.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "layer": {"type": "string", "enum": ["global", "agent", "project", "task"], "description": "Layer of the record to confirm (default agent)."},
                    "namespace": {"type": "string", "description": "Defaults to session namespace; naming another session's namespace is refused."}
                },
                "required": ["key"]
            }),
        },
        Capability::MemoryConfirm,
        move |args| {
            let v = parse_args(args)?;
            let key = v
                .get("key")
                .and_then(|x| x.as_str())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'key'".into(), "check tool arguments and policy"))?
                .to_string();
            let layer = v
                .get("layer")
                .and_then(|x| x.as_str())
                .and_then(layer_str_to_kind)
                .unwrap_or(LayerKind::Agent);
            // Namespace is resolved against the session's own, like every
            // other memory tool: the model must not confirm another
            // session's records by naming its namespace.
            let namespace = resolve_namespace(
                v.get("namespace").and_then(|x| x.as_str()),
                &confirm_opts.namespace,
            )?;
            match confirm_via(
                confirm_opts.store.as_ref(),
                &confirm_opts.policy,
                layer,
                &namespace,
                &key,
            ) {
                Ok(rec) => {
                    confirm_opts.sink.record(MemoryToolEvent::Written {
                        layer: rec.layer,
                        namespace: rec.namespace,
                        key: rec.key.clone(),
                        backend: confirm_opts.backend_label.clone(),
                    });
                    Ok(format!("confirmed {key}: now memory-tier"))
                }
                Err(e) => {
                    confirm_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    Err(e)
                }
            }
        },
    );
}
