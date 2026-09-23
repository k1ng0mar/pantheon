//! AG-UI wire: JSON-RPC commands + SSE frame stream over one seam.
//! Commands mutate runs (send/grant/deny); frames stream back keyed by
//! run_id/thread_id. Signed generative-UI refs ride inside frames.
use crate::rpc::{Dispatcher, MethodHandler, RpcError};
use serde_json::{json, Value};
use std::path::PathBuf;
/// Build a dispatcher with AG-UI commands bound to a data dir.
/// Methods: agui.send, agui.grant, agui.deny, agui.frames, agui.sign, agui.serve_hint.
pub fn dispatcher_for(data_dir: PathBuf) -> Dispatcher {
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
        "agui.frames",
        Frames {
            data_dir: data_dir.clone(),
        },
    );
    d.register(
        "agui.sign",
        Sign {
            data_dir: data_dir.clone(),
        },
    );
    d.register("agui.serve_hint", ServeHint { data_dir });
    d
}
fn data_dir_of(v: &Value) -> Option<PathBuf> {
    v.get("data_dir")
        .and_then(|x| x.as_str())
        .map(PathBuf::from)
}
fn sup_for(dir: &PathBuf) -> Result<pantheon_runtime::Supervisor, RpcError> {
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
        let dir = data_dir_of(&p).unwrap_or_else(|| self.data_dir.clone());
        let run_id = p
            .get("run_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(pantheon_runtime::new_run_id);
        let thread_id = thread_of(&p, &run_id);
        let sup = sup_for(&dir)?;
        if let Some("awaiting_approval") = sup
            .ledger_status(&run_id)
            .map_err(|e| RpcError::internal(e.to_string()))?
            .as_deref()
        {
            return Err(RpcError::invalid_params(format!(
                "run {run_id} is parked on approval; grant/deny first"
            )));
        }
        sup.start_run(&run_id)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        crate::serve::remember_thread(&dir, &run_id, &thread_id);
        sup.emit(pantheon_core::events::Event::RunProgress {
            run_id: run_id.clone(),
            detail: format!("user: {text}"),
        })
        .map_err(|e| RpcError::internal(e.to_string()))?;
        let entries = sup
            .replay(&run_id)
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let frames = pantheon_gateway::frames_for_entries(&entries, &thread_id);
        Ok(json!({"run_id": run_id, "thread_id": thread_id, "frames": frames}))
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
        let dir = data_dir_of(&p).unwrap_or_else(|| self.data_dir.clone());
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
        let scope = p.get("scope").and_then(|x| x.as_str()).unwrap_or("");
        let dir = data_dir_of(&p).unwrap_or_else(|| self.data_dir.clone());
        let sup = sup_for(&dir)?;
        sup.fail(run_id, &format!("APPROVAL_DENIED:{scope}"))
            .map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(json!({"run_id": run_id, "denied": true}))
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
        let dir = data_dir_of(&p).unwrap_or_else(|| self.data_dir.clone());
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
}
impl MethodHandler for Sign {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let task = p
            .get("task_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| RpcError::invalid_params("field \"task_id\" is required"))?;
        if task.contains('/') || task.contains('.') || task.is_empty() {
            return Err(RpcError::invalid_params("bad task_id"));
        }
        let mime = p
            .get("mime")
            .and_then(|x| x.as_str())
            .unwrap_or("application/octet-stream");
        let ttl = p.get("ttl_ms").and_then(|x| x.as_i64()).unwrap_or(3600_000);
        let base = p
            .get("base_url")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "http://127.0.0.1:18789/agui/blob".into());
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
}
impl MethodHandler for ServeHint {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let p = params.unwrap_or(Value::Null);
        let port = p.get("port").and_then(|x| x.as_u64()).unwrap_or(18789);
        let _ = &self.data_dir;
        Ok(
            json!({"sse": format!("http://127.0.0.1:{port}/agui/stream"), "rpc": format!("http://127.0.0.1:{port}/agui/rpc")} ),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{Id, Request};
    fn call(d: &Dispatcher, method: &str, params: Value) -> Value {
        let req = Request {
            jsonrpc: "2.0".into(),
            id: Id::Number(1),
            method: method.into(),
            params: Some(params),
        };
        let resp = d.dispatch(&req).unwrap();
        assert!(resp.is_success(), "{resp:?}");
        resp.result.unwrap()
    }
    #[test]
    fn send_grant_deny_frames_round_trip() {
        let dir = std::env::temp_dir().join(format!("pantheon-agui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dispatcher_for(dir.clone());
        let dd = dir.to_string_lossy().to_string();
        let v = call(
            &d,
            "agui.send",
            json!({"data_dir": dd, "run_id": "r1", "thread_id": "web:t1", "text": "hello"}),
        );
        assert_eq!(v["run_id"], "r1");
        let f = call(
            &d,
            "agui.frames",
            json!({"data_dir": dd, "run_id": "r1", "thread_id": "web:t1"}),
        );
        assert!(f["sse"].as_str().unwrap().contains("event: run"));
        assert!(f["frames"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["thread_id"] == "web:t1"));
        let after = call(
            &d,
            "agui.frames",
            json!({"data_dir": dd, "run_id": "r1", "after": 99999}),
        );
        assert_eq!(after["frames"].as_array().unwrap().len(), 1);
    }
    #[test]
    fn parked_run_refuses_send_until_grant() {
        let dir = std::env::temp_dir().join(format!("pantheon-agui2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dispatcher_for(dir.clone());
        let dd = dir.to_string_lossy().to_string();
        let sup = pantheon_runtime::Supervisor::open(dir).unwrap();
        sup.start_run("park").unwrap();
        sup.emit(pantheon_core::events::Event::ApprovalRequested {
            run_id: "park".into(),
            scope: "call_0_0".into(),
        })
        .unwrap();
        let req = Request {
            jsonrpc: "2.0".into(),
            id: Id::Number(1),
            method: "agui.send".into(),
            params: Some(json!({"data_dir": dd, "run_id": "park", "text": "again"})),
        };
        let resp = d.dispatch(&req).unwrap();
        assert!(!resp.is_success());
        let g = call(
            &d,
            "agui.grant",
            json!({"data_dir": dd, "run_id": "park", "scope": "call_0_0"}),
        );
        assert_eq!(g["granted"], true);
    }
}
