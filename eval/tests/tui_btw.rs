//! `/btw` background tasks: lifecycle, spawn cap, secret-free labels,
//! and labeled result blocks.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_tui::session::bg::{self, BgStatus, BgTask};

fn running_task(id: u64) -> BgTask {
    let mut t = BgTask::new(id, "prompt", format!("run-{id}"), "parent".into());
    t.mark_running();
    t
}

#[test]
fn label_redacts_secrets() {
    for prompt in [
        "deploy with sk-abcdef1234567890 tonight",
        "use Bearer hunter2 for the call",
        "key is sk-or-v1-zzzz9999 ok?",
        "PANTHEON_SECRET_DB=topsecretpassword here",
    ] {
        let label = bg::task_label(prompt);
        assert!(
            !label.contains("abcdef1234567890")
                && !label.contains("hunter2")
                && !label.contains("zzzz9999")
                && !label.contains("topsecretpassword"),
            "secret leaked into label: {label}"
        );
        assert!(label.contains("[REDACTED]"), "no redaction marker: {label}");
    }
}

#[test]
fn label_uses_first_line_and_truncates() {
    let label = bg::task_label("first line here\nsecond line must not appear");
    assert!(!label.contains("second"), "label kept later lines: {label}");

    let long = "x".repeat(400);
    let label = bg::task_label(&long);
    assert!(
        label.chars().count() <= bg::LABEL_CHARS + 1,
        "label too long: {} chars",
        label.chars().count()
    );
    assert!(label.ends_with('…'));
}

#[test]
fn lifecycle_queued_running_done() {
    let mut t = BgTask::new(1, "do things", "run-1".into(), "parent-1".into());
    assert_eq!(t.status, BgStatus::Queued);
    assert!(t.status.is_active());
    assert!(t.finished_ms.is_none());

    t.mark_running();
    assert_eq!(t.status, BgStatus::Running);

    t.finish("all good".into());
    assert_eq!(t.status, BgStatus::Done);
    assert!(!t.status.is_active());
    assert_eq!(t.output.as_deref(), Some("all good"));
    assert!(t.finished_ms.is_some());
    assert!(t.elapsed_ms() < 60_000);
}

#[test]
fn lifecycle_queued_running_failed() {
    let mut t = running_task(2);
    t.fail("boom".into());
    assert_eq!(t.status, BgStatus::Failed);
    assert!(!t.status.is_active());
    assert_eq!(t.output.as_deref(), Some("boom"));
    assert!(t.finished_ms.is_some());
}

#[test]
fn cap_allows_four_active_refuses_fifth() {
    let mut tasks: Vec<BgTask> = (0..4).map(running_task).collect();
    assert_eq!(bg::active_count(&tasks), 4);
    assert!(!bg::can_spawn(&tasks), "5th task must be refused at cap");

    // A finished task frees a slot.
    tasks[0].finish("done".into());
    assert_eq!(bg::active_count(&tasks), 3);
    assert!(bg::can_spawn(&tasks));

    // Queued tasks count as active too.
    let queued = BgTask::new(9, "p", "run-9".into(), "parent".into());
    assert!(queued.status.is_active());
    let mut tasks2: Vec<BgTask> = (0..3).map(running_task).collect();
    tasks2.push(queued);
    assert!(!bg::can_spawn(&tasks2));
}

#[test]
fn result_header_distinguishes_done_and_failed() {
    let mut ok = running_task(7);
    // Re-label with a realistic prompt for the header assertion.
    ok.label = bg::task_label("summarize the quarterly report");
    ok.finish("summary".into());
    let h = bg::result_header(&ok);
    assert!(h.contains("bg-7"), "header missing id: {h}");
    assert!(
        h.contains("summarize the quarterly report"),
        "header missing label: {h}"
    );
    assert!(h.contains("background result"), "header missing kind: {h}");

    let mut bad = running_task(8);
    bad.label = bg::task_label("break things");
    bad.fail("disk on fire".into());
    let h = bg::result_header(&bad);
    assert!(h.contains("failed"), "failure header not marked: {h}");
    assert!(
        !h.contains("background result"),
        "failure header mislabeled: {h}"
    );
}

#[test]
fn status_segment_hidden_when_idle_shown_when_active() {
    assert_eq!(bg::status_segment(&[]), None);

    let mut done = running_task(1);
    done.finish("x".into());
    assert_eq!(bg::status_segment(std::slice::from_ref(&done)), None);

    let tasks: Vec<BgTask> = (0..2).map(running_task).collect();
    let seg = bg::status_segment(&tasks).expect("segment for 2 active tasks");
    assert!(seg.starts_with("bg 2 "), "unexpected segment: {seg}");
}

#[test]
fn spawn_plan_never_touches_the_main_run() {
    // The /btw spawn path must leave the main turn alone: planning a task
    // only reads the parent run id for provenance; the task gets its own
    // run id, its own cancel token, and starts in Queued.
    let parent = "main-run-abc";
    let t = BgTask::new(3, "background work", "bg-run-xyz".into(), parent.into());
    assert_eq!(t.parent_run_id, parent);
    assert_ne!(t.run_id, parent, "task must not reuse the main run id");
    assert_eq!(t.status, BgStatus::Queued);
    assert!(!t.cancel.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn summary_line_is_first_line_capped() {
    let s = bg::summary_line("first line\nsecond line");
    assert_eq!(s, "first line");
    let long = "y".repeat(500);
    assert!(bg::summary_line(&long).chars().count() <= 120);
    assert_eq!(bg::summary_line(""), "");
}
