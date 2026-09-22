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
use pantheon_memory::{propose_write, recall, LayerKind, MemoryStore, Proposal, Provenance};
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
    pub store: Arc<MemoryStore>,
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
            let hits = match recall(
                &recall_opts.store,
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
                out.push_str(&format!(
                    "- [{:?}] {} = {}\n",
                    h.record.layer, h.record.key, h.record.value
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
                    "namespace": {"type": "string", "description": "Defaults to session namespace."},
                    "origin": {"type": "string", "description": "Provenance origin tag."}
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
            let origin = v
                .get("origin")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "model".into());

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

            match propose_write(
                &propose_opts.store,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer_policy() -> pantheon_core::capability::Policy {
        use pantheon_core::capability::Capability as C;
        pantheon_core::capability::Policy::coder().allow(C::MemoryWrite)
    }

    fn read_policy() -> pantheon_core::capability::Policy {
        pantheon_core::capability::Policy::coder()
    }

    fn fresh(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-memtools-{}-{}-{}",
            name,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn opts_with(policy: pantheon_core::capability::Policy) -> (MemoryToolOptions, VecMemorySink) {
        let dir = fresh("opt");
        let store = Arc::new(MemoryStore::open(&dir.join("memory.db")).unwrap());
        let sink = VecMemorySink::new();
        let opts = MemoryToolOptions {
            store,
            policy: Arc::new(policy),
            namespace: "nyx".into(),
            max_bytes: 4096,
            sink: Arc::new(sink.clone()),
            backend_label: "native".into(),
        };
        (opts, sink)
    }

    fn build_registry(opts: MemoryToolOptions) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        register_memory_tools(&mut reg, opts);
        reg
    }

    #[test]
    fn recall_returns_matches_with_provenance_and_layers() {
        let (opts, sink) = opts_with(writer_policy());
        let reg = build_registry(opts);
        reg.execute(
            "memory_propose",
            r#"{"key":"city","value":"Kano","layer":"agent"}"#,
        )
        .unwrap();
        reg.execute(
            "memory_propose",
            r#"{"key":"tz","value":"Africa/Lagos","layer":"agent"}"#,
        )
        .unwrap();
        let out = reg.execute("memory_recall", r#"{"query":"Kano"}"#).unwrap();
        assert!(out.contains("city"));
        assert!(out.contains("Kano"));
        let events = sink.events.lock().unwrap();
        let recalled = events
            .iter()
            .filter(|e| matches!(e, MemoryToolEvent::Recalled { .. }))
            .count();
        assert!(recalled >= 1);
    }

    #[test]
    fn list_shows_every_agent_record() {
        let (opts, _) = opts_with(writer_policy());
        let reg = build_registry(opts);
        reg.execute(
            "memory_propose",
            r#"{"key":"a","value":"1","layer":"agent"}"#,
        )
        .unwrap();
        reg.execute(
            "memory_propose",
            r#"{"key":"b","value":"2","layer":"agent"}"#,
        )
        .unwrap();
        let out = reg.execute("memory_list", "{}").unwrap();
        assert!(out.contains("a = 1"));
        assert!(out.contains("b = 2"));
    }

    #[test]
    fn propose_runs_through_policy_gate() {
        let (opts, sink) = opts_with(read_policy()); // read-only, no MemoryWrite
        let reg = build_registry(opts);
        let err = reg
            .execute(
                "memory_propose",
                r#"{"key":"city","value":"Kano","layer":"agent"}"#,
            )
            .unwrap_err();
        assert_eq!(err.code, "MEM_NO_CAPABILITY");
        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(
            |e| matches!(e, MemoryToolEvent::Denied { code, .. } if code == "MEM_NO_CAPABILITY")
        ));
    }

    #[test]
    fn propose_rejects_empty_value() {
        let (opts, _) = opts_with(writer_policy());
        let reg = build_registry(opts);
        let err = reg
            .execute(
                "memory_propose",
                r#"{"key":"k","value":"   ","layer":"agent"}"#,
            )
            .unwrap_err();
        assert_eq!(err.code, "MEM_EMPTY");
    }

    #[test]
    fn propose_respects_max_bytes() {
        let (opts, _) = opts_with(writer_policy());
        let big = "x".repeat(opts.max_bytes + 1);
        let reg = build_registry(opts);
        let err = reg
            .execute(
                "memory_propose",
                &format!(r#"{{"key":"k","value":"{big}","layer":"agent"}}"#),
            )
            .unwrap_err();
        assert_eq!(err.code, "MEM_TOO_LARGE");
    }

    #[test]
    fn forget_removes_record_and_emits_event() {
        let (opts, sink) = opts_with(writer_policy());
        let reg = build_registry(opts);
        reg.execute(
            "memory_propose",
            r#"{"key":"k","value":"v","layer":"agent"}"#,
        )
        .unwrap();
        let out = reg
            .execute("memory_forget", r#"{"key":"k","layer":"agent"}"#)
            .unwrap();
        assert!(out.contains("forgot k"));
        // The lock is dropped before the next tool call. Holding it across
        // a `memory_*` execute would deadlock: the tool closure also
        // touches the sink and std::sync::Mutex is not re-entrant.
        {
            let events = sink.events.lock().unwrap();
            assert!(events
                .iter()
                .any(|e| matches!(e, MemoryToolEvent::Forgotten { key, .. } if key == "k")));
        }
        // Subsequent recall must not return it.
        let rec = reg.execute("memory_recall", r#"{"query":"v"}"#).unwrap();
        assert!(rec.contains("(no matches)"));
    }

    #[test]
    fn forget_unknown_key_returns_structured_error() {
        let (opts, _) = opts_with(writer_policy());
        let reg = build_registry(opts);
        let err = reg
            .execute("memory_forget", r#"{"key":"missing"}"#)
            .unwrap_err();
        assert_eq!(err.code, "MEM_NOT_FOUND");
    }

    #[test]
    fn unknown_layer_string_falls_back_to_agent() {
        let (opts, _) = opts_with(writer_policy());
        let reg = build_registry(opts);
        reg.execute(
            "memory_propose",
            r#"{"key":"k","value":"v","layer":"banana"}"#,
        )
        .unwrap();
        let list = reg.execute("memory_list", "{}").unwrap();
        assert!(list.contains("k = v"));
    }

    #[test]
    fn all_tools_have_known_capabilities() {
        let (opts, _) = opts_with(writer_policy());
        let reg = build_registry(opts);
        for name in [
            "memory_recall",
            "memory_list",
            "memory_propose",
            "memory_forget",
        ] {
            let cap = reg.capability_of(name).expect(name);
            assert!(
                matches!(cap, Capability::MemoryRead | Capability::MemoryWrite),
                "{name} registered with unexpected capability: {cap:?}"
            );
        }
    }
}
