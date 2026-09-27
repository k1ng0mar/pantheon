//! Tests for `pantheon_api::agui::tests` — sibling file so sources stay test-free.
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
fn serve_hint_uses_configured_port() {
    let dir = std::env::temp_dir().join(format!("pantheon-agui-port-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dispatcher_for_with_hint(dir, 43219);
    let v = call(&d, "agui.serve_hint", json!({"port": 1}));
    assert_eq!(v["sse"], "http://127.0.0.1:43219/agui/stream");
}
#[test]
fn scoped_denial_keeps_run_active() {
    let dir = std::env::temp_dir().join(format!("pantheon-agui-deny-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dispatcher_for(dir.clone());
    let dd = dir.to_string_lossy().to_string();
    let sup = crate::Supervisor::open(dir).unwrap();
    sup.start_run("deny-run").unwrap();
    sup.emit(pantheon_api::events::Event::ApprovalRequested {
        run_id: "deny-run".into(),
        scope: "call_0_0".into(),
    })
    .unwrap();
    let v = call(
        &d,
        "agui.deny",
        json!({"data_dir": dd, "run_id": "deny-run", "scope": "call_0_0"}),
    );
    assert_eq!(v["denied"], true);
    assert_eq!(v["run_status"], "running");
    assert_eq!(
        sup.ledger_status("deny-run").unwrap().as_deref(),
        Some("running")
    );
}
#[test]
fn scoped_denial_keeps_other_approval_pending() {
    let dir = std::env::temp_dir().join(format!("pantheon-agui-multi-deny-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = crate::Supervisor::open(dir).unwrap();
    sup.start_run("multi-deny").unwrap();
    for scope in ["a", "b"] {
        sup.emit(pantheon_api::events::Event::ApprovalRequested {
            run_id: "multi-deny".into(),
            scope: scope.into(),
        })
        .unwrap();
    }
    sup.deny("multi-deny", "a").unwrap();
    assert_eq!(
        sup.ledger_status("multi-deny").unwrap().as_deref(),
        Some("awaiting_approval")
    );
    sup.grant("multi-deny", "b").unwrap();
    assert_eq!(
        sup.ledger_status("multi-deny").unwrap().as_deref(),
        Some("running")
    );
}
#[test]
fn serve_hint_uses_configured_host() {
    let dir = std::env::temp_dir().join(format!("pantheon-agui-host-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dispatcher_for_with_hint_and_host(
        dir,
        43219,
        "http://127.0.0.1:43219/agui/blob".into(),
        "0.0.0.0",
    );
    let v = call(&d, "agui.serve_hint", json!({}));
    assert_eq!(v["sse"], "http://127.0.0.1:43219/agui/stream");
}

#[test]
fn artifact_put_returns_a_signed_reference() {
    let dir = std::env::temp_dir().join(format!("pantheon-artifact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dispatcher_for_with_hint_and_base(
        dir.clone(),
        43219,
        "http://127.0.0.1:43219/agui/blob".into(),
    );
    let dd = dir.to_string_lossy().to_string();
    let value = call(
        &d,
        "agui.artifact.put",
        json!({"data_dir": dd, "task_id": "task_1", "mime": "text/plain", "text": "hello"}),
    );
    assert!(value["url"]
        .as_str()
        .unwrap()
        .contains("/agui/blob/task_1?"));
    let artifact = crate::Supervisor::open(dir)
        .unwrap()
        .artifact("task_1")
        .unwrap()
        .unwrap();
    assert_eq!(artifact.bytes, b"hello");
}

#[test]
fn parked_run_refuses_send_until_grant() {
    let dir = std::env::temp_dir().join(format!("pantheon-agui2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dispatcher_for(dir.clone());
    let dd = dir.to_string_lossy().to_string();
    let sup = crate::Supervisor::open(dir).unwrap();
    sup.start_run("park").unwrap();
    sup.emit(pantheon_api::events::Event::ApprovalRequested {
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

#[test]
fn turn_locks_are_per_run_and_released() {
    let a1 = super::turn_lock_for("run-a");
    let b = super::turn_lock_for("run-b");
    {
        let a2 = super::turn_lock_for("run-a");
        assert!(Arc::ptr_eq(&a1, &a2), "same run shares one lock");
        assert!(!Arc::ptr_eq(&a1, &b), "different runs get different locks");
    } // a2 dropped: only the map + a1/b hold the locks now
    // Nobody holds or waits on them: both entries are pruned.
    super::release_turn_lock("run-a", &a1);
    super::release_turn_lock("run-b", &b);
    let map = super::TURN_LOCKS.lock().unwrap();
    assert!(!map.contains_key("run-a"));
    assert!(!map.contains_key("run-b"));
}

#[test]
fn per_run_turn_lock_serializes_workers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let lock = super::turn_lock_for("run-serial");
    let inside = Arc::new(AtomicUsize::new(0));
    let max_inside = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];
    for _ in 0..8 {
        let lock = lock.clone();
        let inside = inside.clone();
        let max_inside = max_inside.clone();
        handles.push(std::thread::spawn(move || {
            let _guard = lock.lock().unwrap();
            let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
            max_inside.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            inside.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(max_inside.load(Ordering::SeqCst), 1, "turns overlapped");
    super::release_turn_lock("run-serial", &lock);
}
