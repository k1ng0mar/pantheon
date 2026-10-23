//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral - SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem - lives here and
//! runs via `cargo test -p pantheon-eval`.

use pantheon_exec::skills::*;
use std::io::Write;

fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        for (name, data) in entries {
            w.start_file(
                *name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

fn make_zip_from_strings(entries: &[(String, &[u8])]) -> Vec<u8> {
    let strs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_str(), *d)).collect();
    make_zip(&strs)
}

#[test]
fn skill_zip_rejects_path_traversal_entries() {
    // A hostile registry can ship a ZIP whose entry names walk up the
    // tree. `unzip_skill` is the one gate between those names and
    // `write_bundle`, so the guard is the whole defense: both `..` and
    // absolute entries must fail before anything is returned, and a
    // clean bundle must still inflate normally.
    let evil = make_zip(&[("../escape", b"x"), ("./ok", b"y")]);
    let err = unzip_skill(&evil).expect_err("the traversal entry must be rejected");
    assert!(err.contains("unsafe entry path"), "got: {err}");

    let abs = make_zip(&[("/abs/escape", b"x")]);
    let err = unzip_skill(&abs).expect_err("the absolute entry must be rejected");
    assert!(err.contains("unsafe entry path"), "got: {err}");

    // A well-formed bundle still passes through untouched.
    let ok = make_zip(&[
        ("SKILL.md", b"---\nname: x\n---\n"),
        ("scripts/run.sh", b"#!/bin/sh"),
    ]);
    let infl = unzip_skill(&ok).expect("the clean bundle must inflate");
    let names: Vec<String> = infl.iter().map(|(p, _)| p.display().to_string()).collect();
    assert!(names.iter().any(|n| n.contains("SKILL.md")));
    assert!(names.iter().any(|n| n.contains("run.sh")));
}

#[test]
fn skill_zip_rejects_entry_count_overrun() {
    // Zip-bomb guard #1: cap the number of entries in one bundle.
    let many = (0..(MAX_ZIP_ENTRIES + 1))
        .map(|i| (format!("e{i}"), b"x" as &[u8]))
        .collect::<Vec<_>>();
    let err = unzip_skill(&make_zip_from_strings(&many)).expect_err("the entry cap must trip");
    assert!(err.contains("entries"), "got: {err}");
}

#[test]
fn skill_zip_rejects_total_inflated_bytes_overrun() {
    // Zip-bomb guard: cap total inflated bytes on actual bytes read, not
    // the header's claim. Two 10-byte entries past a 5-byte cap trip it.
    let limits = ZipLimits {
        max_entries: 1000,
        max_total_bytes: 5,
    };
    let two = make_zip(&[("a", b"0123456789"), ("b", b"0123456789")]);
    let err = unzip_skill_with_limits(&two, &limits).expect_err("the total-bytes cap must trip");
    assert!(err.contains("inflates past"), "got: {err}");
}

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
