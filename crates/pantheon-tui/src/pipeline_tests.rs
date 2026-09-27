//! Tests for `crate::pipeline::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn spec_round_trips_through_the_operations_store() {
    // The approve path resumes from this: park persists the spec, the
    // approving process loads it. If either side breaks, approvals
    // become dead letters again.
    let dir = std::env::temp_dir().join(format!("pantheon-pipe-spec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sup = Supervisor::open(dir.clone()).unwrap();
    assert_eq!(load_spec(&sup, "run1"), None);
    sup.operations()
        .create(
            spec_op_id("run1"),
            "pipeline.spec",
            serde_json::json!({"spec": "do the thing"}),
        )
        .unwrap();
    assert_eq!(load_spec(&sup, "run1").as_deref(), Some("do the thing"));
    let _ = std::fs::remove_dir_all(&dir);
}
