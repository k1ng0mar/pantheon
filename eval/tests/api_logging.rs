//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::logging::rotate_log;
use std::path::Path;

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join("logs").join(name)).unwrap_or_default()
}

#[test]
fn rotation_renames_generations_in_order_and_drops_the_oldest() {
    let dir = std::env::temp_dir().join(format!("pantheon-log-rot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.log");
    std::fs::write(&path, "v0").unwrap();
    std::fs::write(dir.join("agent.log.1"), "v1").unwrap();
    std::fs::write(dir.join("agent.log.2"), "v2").unwrap();
    std::fs::write(dir.join("agent.log.3"), "v3").unwrap(); // oldest, must be dropped

    rotate_log(&path, 3);

    assert_eq!(
        std::fs::read_to_string(dir.join("agent.log.1")).unwrap(),
        "v0"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("agent.log.2")).unwrap(),
        "v1"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("agent.log.3")).unwrap(),
        "v2"
    );
    assert!(
        !dir.join("agent.log").exists(),
        "current file must move to .1"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
