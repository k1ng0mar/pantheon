//! Model-facing memory tools: four tools the agent can call to recall,
//! list, propose, and forget records. All writes go through
//! `propose -> policy -> provenance -> validation` and emit ledger events.
//! Reads respect `MemoryRead`. Writes require `MemoryWrite` and may
//! require approval per the runtime policy.
//!
//! The tools never touch the SQLite layer directly; they call the same
//! `MemoryBackend` capability gate the CLI does. A backend swap (native
//! store, GalaxyMem, Mnemosyne, Honcho, Hindsight) only changes the
//! `MemoryStore` implementation passed in here.
use crate::tools::{parse_args, ToolRegistry};
use pantheon_core::capability::Capability;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::ToolSchema;
use pantheon_memory::{
    confirm_via, recall_via, write_via, LayerKind, MemoryBackend, Proposal, Provenance,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check tool arguments and policy",
        "",
    )
}

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
/// across the four tools. `max_bytes` caps proposal sizes the same way
/// the CLI does.
#[derive(Clone)]
pub struct MemoryToolOptions {
    /// Active backend (native store or any registered plugin backend:
    /// GalaxyMem, Mnemosyne, Honcho, Hindsight, OpenViking, http bridge).
    /// Gating runs through `recall_via`/`write_via`/`confirm_via` before
    /// the backend sees anything.
    pub store: Arc<dyn MemoryBackend>,
    pub policy: Arc<pantheon_core::capability::Policy>,
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
            trust: pantheon_core::provenance::TrustTier::Untrusted,
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

/// Register `memory_recall`, `memory_list`, `memory_propose`,
/// `memory_forget` on a registry.
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
                .ok_or_else(|| merr("TOOL_BAD_ARGS", "missing 'query'".into()))?
                .to_string();
            let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(8) as usize;
            let layers = [
                LayerKind::Project,
                LayerKind::Agent,
                LayerKind::Global,
            ];
            let hits = match recall_via(
                recall_opts.store.as_ref(),
                &recall_opts.policy,
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
                    pantheon_core::provenance::TrustTier::Untrusted => {
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
            description: "List every Agent-layer record in the current namespace.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        Capability::MemoryRead,
        move |args| {
            let _ = parse_args(args)?;
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
            list_opts.sink.record(MemoryToolEvent::Listed {
                namespace: list_opts.namespace.clone(),
                rows: rows.len(),
            });
            let mut out = String::new();
            for (k, v) in &rows {
                out.push_str(&format!("- {k} = {v}\n"));
            }
            if out.is_empty() {
                out.push_str("(no agent records)\n");
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
                .ok_or_else(|| merr("TOOL_BAD_ARGS", "missing 'key'".into()))?
                .to_string();
            let value = v
                .get("value")
                .and_then(|x| x.as_str())
                .ok_or_else(|| merr("TOOL_BAD_ARGS", "missing 'value'".into()))?
                .to_string();
            let layer = v
                .get("layer")
                .and_then(|x| x.as_str())
                .and_then(layer_str_to_kind)
                .unwrap_or(LayerKind::Agent);
            let namespace = v
                .get("namespace")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| propose_opts.namespace.clone());
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
                .ok_or_else(|| merr("TOOL_BAD_ARGS", "missing 'key'".into()))?
                .to_string();
            let layer = v
                .get("layer")
                .and_then(|x| x.as_str())
                .and_then(layer_str_to_kind)
                .unwrap_or(LayerKind::Agent);
            let namespace = v
                .get("namespace")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| forget_opts.namespace.clone());

            forget_opts.sink.record(MemoryToolEvent::Forgotten {
                layer,
                namespace: namespace.clone(),
                key: key.clone(),
            });

            match forget_opts.store.forget(layer, &namespace, &key) {
                Ok(true) => Ok(format!("forgot {key}")),
                Ok(false) => Err(merr(
                    "MEM_NOT_FOUND",
                    format!("no record for {key} in {namespace}"),
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

    // memory_confirm: the user-promotion path. The agent can only call
    // this when the user has explicitly vouched for a record (via an
    // approval or a direct instruction); the tool elevates an Untrusted
    // record to Memory tier. It cannot exceed Memory tier: System and
    // User are reserved for harness and human authors.
    let confirm_opts = opts.clone();
    reg.register(
        ToolSchema {
            name: "memory_confirm".into(),
            description: "Mark an existing memory record as user-confirmed. Call only after the user explicitly vouched for the record's content; unconfirmed records stay untrusted.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "namespace": {"type": "string", "description": "Defaults to session namespace."}
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
                .ok_or_else(|| merr("TOOL_BAD_ARGS", "missing 'key'".into()))?
                .to_string();
            let namespace = v
                .get("namespace")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| confirm_opts.namespace.clone());
            match confirm_via(
                confirm_opts.store.as_ref(),
                &confirm_opts.policy,
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

#[cfg(test)]
#[path = "memory_tools_tests.rs"]
mod tests;
