//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral — SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem — lives here and
//! runs via `cargo test -p pantheon-eval`.

use pantheon_exec::skills::*;

#[test]
fn import_skill_dir_rejects_traversal_name() {
    let dir = std::env::temp_dir().join(format!("skimp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), "---\nname: x\n---\nbody\n").unwrap();
    let data = dir.join("data");
    for bad in ["../evil", "..", "/abs", "a/b", ""] {
        let err = import_skill_dir(&data, &src, bad).unwrap_err();
        assert_eq!(err.code, "SKILL_BAD_NAME", "{bad:?}");
    }
    // Nothing escaped the data dir: the rejection happens before any write.
    assert!(!dir.join("evil").exists());
    assert!(!data.join("skills").join("evil").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_skill_rejects_bad_meta_name() {
    let dir = std::env::temp_dir().join(format!("skimp2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let skill = Skill {
        meta: SkillMeta {
            name: "../../evil".into(),
            description: String::new(),
            origin: String::new(),
            exec: Vec::new(),
        },
        path: dir.join("x"),
        dir: dir.join("x"),
    };
    let err = import_skill(&dir.join("data"), &skill).unwrap_err();
    assert_eq!(err.code, "SKILL_BAD_NAME");
    assert!(!dir.join("evil").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
