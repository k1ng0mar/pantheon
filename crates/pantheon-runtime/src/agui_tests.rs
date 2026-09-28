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
fn serve_hint_uses_configured_port() {
    let dir = std::env::temp_dir().join(format!("pantheon-agui-port-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let d = dispatcher_for_with_hint(dir, 43219);
    let v = call(&d, "agui.serve_hint", json!({"port": 1}));
    assert_eq!(v["sse"], "http://127.0.0.1:43219/agui/stream");
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
