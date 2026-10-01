//! Runs-handler fail-closed behavior, exercised over real HTTP.
//!
//! Distilled from the dashboard's handler tests: every mutation path
//! that cannot act returns 404/400 *before* touching the spawn
//! machinery, and cancelling an idle run is a clean 200. Paths that
//! would spawn a real turn child are deliberately not exercised here
//! (no subprocesses in eval).

#[path = "support.rs"]
mod support;

use support::boot;

#[test]
fn send_message_unknown_run_is_404() {
    let d = boot();
    let r = d.post("/api/runs/does-not-exist/message", r#"{"message": "hi"}"#);
    assert_eq!(r.status, 404, "{}", r.body);
}

#[test]
fn send_message_rejects_missing_or_empty_message() {
    let d = boot();
    // Unknown run + empty body: the body check comes first.
    let r = d.post("/api/runs/nope/message", "{}");
    assert_eq!(r.status, 400, "{}", r.body);
    let r = d.post("/api/runs/nope/message", r#"{"message": "   "}"#);
    assert_eq!(r.status, 400, "{}", r.body);
}

#[test]
fn cancel_unknown_run_is_404() {
    let d = boot();
    assert_eq!(d.post("/api/runs/does-not-exist/cancel", "").status, 404);
}

#[test]
fn cancel_idle_run_is_200() {
    let d = boot();
    // Admit a run straight into the supervisor: no turn ever starts, so
    // nothing can spawn.
    let sup = pantheon_runtime::Supervisor::open(d.dir.path().to_path_buf()).unwrap();
    let run_id = pantheon_runtime::new_run_id();
    sup.start_run(&run_id).unwrap();
    let r = d.post(&format!("/api/runs/{run_id}/cancel"), "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["ok"], true);
}

#[test]
fn kill_unknown_run_is_404() {
    let d = boot();
    assert_eq!(d.post("/api/runs/does-not-exist/kill", "").status, 404);
}

#[test]
fn retry_unknown_run_is_404() {
    let d = boot();
    assert_eq!(d.post("/api/runs/does-not-exist/retry", "").status, 404);
}

#[test]
fn answer_input_unknown_run_is_404() {
    let d = boot();
    let r = d.post(
        "/api/runs/does-not-exist/input",
        r#"{"call_id": "c1", "answer": "yes"}"#,
    );
    assert_eq!(r.status, 404, "{}", r.body);
}

#[test]
fn clear_queue_unknown_run_is_404() {
    let d = boot();
    assert_eq!(d.delete("/api/runs/does-not-exist/queue").status, 404);
}

#[test]
fn run_detail_unknown_run_is_404() {
    let d = boot();
    assert_eq!(d.get("/api/runs/does-not-exist").status, 404);
}
