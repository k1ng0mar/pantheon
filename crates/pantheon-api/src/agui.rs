//! AG-UI wire: JSON-RPC commands + SSE frame stream over one seam.
//! Commands mutate runs (send/grant/deny); frames stream back keyed by
//! run_id/thread_id. Signed generative-UI refs ride inside frames.
use crate::rpc::{Dispatcher, MethodHandler, RpcError};
use pantheon_gateway::{valid_task_id, GenUiSigner};
use serde_json::{json, Value};
use std::path::PathBuf;
/// Build a dispatcher with AG-UI commands bound to a data dir.
/// Methods: agui.send, agui.grant, agui.deny, agui.frames, agui.sign, agui.serve_hint.
pub fn dispatcher_for(data_dir: PathBuf) -> Dispatcher {
    dispatcher_for_with_hint(data_dir, 18789)
}

/// Build the AG-UI dispatcher with the actual serving port.  The default
/// constructor remains for embedders, but a real server must use this form so
/// `agui.serve_hint` never points clients at the development port.
pub fn dispatcher_for_with_hint(data_dir: PathBuf, port: u16) -> Dispatcher {
    dispatcher_for_with_hint_and_base(data_dir, port, format!("http://127.0.0.1:{port}/agui/blob"))
}

pub fn dispatcher_for_with_hint_and_base(
    data_dir: PathBuf,
    port: u16,
    genui_base: String,
) -> Dispatcher {
    dispatcher_for_with_hint_and_host(data_dir, port, genui_base, "127.0.0.1")
}

pub fn dispatcher_for_with_hint_and_host(
    data_dir: PathBuf,
    port: u16,
    genui_base: String,
    host: &str,
) -> Dispatcher {
    let d = Dispatcher::with_builtins();
    d.register(
        "agui.send",
        SendMsg {
            data_dir: data_dir.clone(),
        },
    );
    d.register(
        "agui.grant",
        Grant {
            data_dir: data_dir.clone(),
        },
    );
    d.register(
        "agui.deny",
        Deny {
            data_dir: data_dir.clone(),
        },
    );
    d.register(
        "agui.cancel",
        Cancel {
            data_dir: data_dir.clone(),
        },
    );
    d.register(
        "agui.artifact.put",
        PutArtifact {
            data_dir: data_dir.clone(),
            default_base: genui_base.clone(),
        },
    );
    d.register(
        "agui.frames",
        Frames {
            data_dir: data_dir.clone(),
        },
    );
    d.register(
        "agui.sign",
        Sign {
            data_dir: data_dir.clone(),
            default_base: genui_base.clone(),
        },
    );
    d.register(
        "agui.serve_hint",
        ServeHint {
            data_dir,
            port,
            host: host.to_string(),
        },
    );
    d
}
fn sup_for(dir: &PathBuf) -> Result<pantheon_runtime::Supervisor, RpcError> {
    // NOTE (group-C): one fresh Supervisor per RPC call — 3 SQLite
    // connections + migrations, then dropped. Fine for a local
    // single-user server (milliseconds); introduce a SupervisorPool
    // if this ever goes multi-user. Do NOT cache across threads
    // without it: the ledger uses immediate transactions + a Mutex.
    pantheon_runtime::Supervisor::open(dir.clone())
        .map_err(|e| RpcError::internal(format!("open runtime: {e}")))
}
fn thread_of(params: &Value, run_id: &str) -> String {
    if let Some(t) = params.get("thread_id").and_then(|x| x.as_str()) {
        return t.to_string();
    }
    format!("cli:{run_id}")
}
struct SendMsg {
    data_dir: PathBuf,
}
impl MethodHandler for SendMsg {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let text = p.get("text").and_then(|x| x.as_str()).unwrap_or("");
        if text.is_empty() {
            return Err(RpcError::invalid_params("field \"text\" is required"));
        }
        let dir = self.data_dir.clone();
        let run_id = p
            .get("run_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(pantheon_runtime::new_run_id);
        let thread_id = thread_of(&p, &run_id);
        let sup = sup_for(&dir)?;
        if let Some(status) = sup
            .ledger_status(&run_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .as_deref()
        {
            if status == "awaiting_approval" {
                return Err(RpcError::invalid_params(format!(
                    "run {run_id} is parked on approval; grant/deny first"
                )));
            }
        }
        let session = pantheon_runtime::session::Session::from_env(dir.clone())
            .map_err(|e| RpcError::internal(format!("open session: {e}")))?;
        crate::serve::remember_thread(&dir, &run_id, &thread_id);
        let turn_id = pantheon_runtime::new_turn_id();
        let worker_run = run_id.clone();
        let worker_turn = turn_id.clone();
        let worker_text = text.to_string();
        std::thread::spawn(move || {
            if let Err(error) = session.chat_turn(&worker_run, &worker_turn, &worker_text) {
                eprintln!("agui turn {worker_turn} failed: {error}");
            }
        });
        // Admission is durable before the RPC returns. A short bounded wait
        // gives the client an initial replay cursor without coupling admission
        // to model/tool completion.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        loop {
            let entries = sup
                .replay(&run_id)
                .map_err(|e| RpcError::internal(e.to_string()))?;
            if entries.iter().any(|entry| {
                matches!(
                    &entry.event,
                    pantheon_core::events::Event::TurnStarted { turn_id: id, .. } if id == &turn_id
                )
            }) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let entries = sup
            .replay(&run_id)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let frames = pantheon_gateway::frames_for_entries(&entries, &thread_id);
        Ok(json!({
            "run_id": run_id,
            "turn_id": turn_id,
            "thread_id": thread_id,
            "accepted": true,
            "frames": frames
        }))
    }
}
struct Grant {
    data_dir: PathBuf,
}
impl MethodHandler for Grant {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let run_id = p
            .get("run_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"run_id\" is required"))?;
        let scope = p
            .get("scope")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"scope\" is required"))?;
        let dir = self.data_dir.clone();
        let sup = sup_for(&dir)?;
        sup.grant(run_id, scope)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        Ok(json!({"run_id": run_id, "scope": scope, "granted": true}))
    }
}
struct Deny {
    data_dir: PathBuf,
}
impl MethodHandler for Deny {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let run_id = p
            .get("run_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"run_id\" is required"))?;
        let scope = p
            .get("scope")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"scope\" is required"))?;
        let dir = self.data_dir.clone();
        let sup = sup_for(&dir)?;
        sup.deny(run_id, scope)
            .map_err(|e| RpcError::invalid_params(e.to_string()))?;
        let run_status = sup
            .ledger_status(run_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .unwrap_or_else(|| "unknown".into());
        Ok(json!({"run_id": run_id, "scope": scope, "denied": true, "run_status": run_status}))
    }
}
struct Cancel {
    data_dir: PathBuf,
}
impl MethodHandler for Cancel {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let run_id = p
            .get("run_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"run_id\" is required"))?
            .to_string();
        let reason = p
            .get("reason")
            .and_then(|x| x.as_str())
            .unwrap_or("user requested cancel")
            .to_string();
        let dir = self.data_dir.clone();
        let sup = sup_for(&dir)?;
        // Phase 1: record cancellation intent synchronously so the run
        // stops taking new work immediately. This is fast (ledger writes).
        sup.cancel_run_intent(&run_id, &reason)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        // Phase 2: process-group TERM/KILL can block up to the grace period
        // per group, so it runs off the HTTP thread. The heartbeat in the
        // lease guard also watches for the canceled status.
        let worker_run = run_id.clone();
        let worker_reason = reason.clone();
        let worker = std::thread::spawn(move || {
            let _ = sup.finish_cancel(&worker_run, &worker_reason);
        });
        // Do not wait for termination: the RPC replies the moment intent is
        // durable. Join detached-style; a stuck TERM escalates to KILL.
        std::mem::forget(worker);
        Ok(json!({
            "run_id": run_id,
            "canceled": true,
            "terminating": true
        }))
    }
}

struct PutArtifact {
    data_dir: PathBuf,
    default_base: String,
}
impl MethodHandler for PutArtifact {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let task = p
            .get("task_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"task_id\" is required"))?;
        if !valid_task_id(task) {
            return Err(RpcError::invalid_params("bad task_id"));
        }
        let mime = p
            .get("mime")
            .and_then(|x| x.as_str())
            .unwrap_or("application/octet-stream");
        let bytes = if let Some(text) = p.get("text").and_then(|x| x.as_str()) {
            text.as_bytes().to_vec()
        } else if let Some(array) = p.get("bytes").and_then(|x| x.as_array()) {
            let mut out = Vec::with_capacity(array.len());
            for n in array {
                out.push(
                    n.as_u64()
                        .filter(|v| *v <= 255)
                        .ok_or_else(|| RpcError::invalid_params("bytes must be 0..255"))?
                        as u8,
                );
            }
            out
        } else {
            return Err(RpcError::invalid_params(
                "field \"text\" or \"bytes\" is required",
            ));
        };
        let dir = self.data_dir.clone();
        sup_for(&dir)?
            .put_artifact(task, mime, &bytes)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let secret = std::env::var("PANTHEON_GENUI_SECRET")
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| b"pantheon-dev-genui-secret".to_vec());
        let base = &self.default_base;
        let r = GenUiSigner::new(base, secret).sign(task, mime, 3_600_000);
        Ok(json!({"task_id": r.task_id, "url": r.url, "expires_ms": r.expires_ms, "mime": r.mime}))
    }
}

struct Frames {
    data_dir: PathBuf,
}
impl MethodHandler for Frames {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let run_id = p
            .get("run_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"run_id\" is required"))?;
        let dir = self.data_dir.clone();
        let thread_id = thread_of(&p, run_id);
        let after = p.get("after").and_then(|x| x.as_i64()).unwrap_or(0);
        let sup = sup_for(&dir)?;
        let entries = sup
            .replay(run_id)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let mut frames = pantheon_gateway::frames_for_entries(&entries, &thread_id);
        frames.retain(|f| f.id > after || f.id == 0);
        let sse = pantheon_gateway::SseEncoder.frames(&frames);
        Ok(json!({"run_id": run_id, "thread_id": thread_id, "frames": frames, "sse": sse}))
    }
}
struct Sign {
    data_dir: PathBuf,
    default_base: String,
}
impl MethodHandler for Sign {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let task = p
            .get("task_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"task_id\" is required"))?;
        if !valid_task_id(task) {
            return Err(RpcError::invalid_params("bad task_id"));
        }
        let mime = p
            .get("mime")
            .and_then(|x| x.as_str())
            .unwrap_or("application/octet-stream");
        let ttl = p.get("ttl_ms").and_then(|x| x.as_i64()).unwrap_or(3600_000);
        if ttl <= 0 {
            return Err(RpcError::invalid_params("ttl_ms must be positive"));
        }
        let base = self.default_base.clone();
        let secret = std::env::var("PANTHEON_GENUI_SECRET")
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| b"pantheon-dev-genui-secret".to_vec());
        let signer = pantheon_gateway::GenUiSigner::new(base, secret);
        let r = signer.sign(task, mime, ttl);
        let _ = &self.data_dir;
        Ok(json!({"task_id": r.task_id, "url": r.url, "expires_ms": r.expires_ms, "mime": r.mime}))
    }
}
struct ServeHint {
    data_dir: PathBuf,
    port: u16,
    host: String,
}
impl MethodHandler for ServeHint {
    fn call(&self, _params: Option<Value>) -> Result<Value, RpcError> {
        let port = self.port;
        let host = match self.host.as_str() {
            "0.0.0.0" | "" => "127.0.0.1",
            "[::]" | "::" => "[::1]",
            "::1" => "[::1]",
            host => host,
        };
        let _ = &self.data_dir;
        Ok(json!({
            "sse": format!("http://{host}:{port}/agui/stream"),
            "rpc": format!("http://{host}:{port}/agui/rpc"),
        }))
    }
}
#[cfg(test)]
#[path = "agui_tests.rs"]
mod tests;
