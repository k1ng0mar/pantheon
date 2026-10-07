//! Zip-attachment extraction (`pantheon-dashboard::uploads`).
//!
//! A zip attached to a chat message is inflated server-side at send time
//! so the agent can read the contents with its file tools. These tests
//! pin the fail-closed behavior: magic-byte detection, zip-slip
//! rejection, the entry-count cap, idempotent re-extraction, and the
//! symlink skip.

use pantheon_dashboard::uploads::{
    extract_zip_upload, is_zip_bytes, is_zip_upload, zip_extract_dir, UploadInfo, MAX_ZIP_ENTRIES,
};
use pantheon_dashboard::App;
use std::io::Write;
use std::path::PathBuf;

fn test_app(dir: &std::path::Path) -> App {
    App {
        data_dir: dir.to_path_buf(),
        token: "test".to_string(),
        bind: "127.0.0.1".to_string(),
        bind_all: false,
        on_approval: None,
        send_locks: Default::default(),
        turn_children: Default::default(),
        swarm: std::sync::Arc::new(pantheon_runtime::swarm_exec::SwarmOrchestrator::new(
            std::sync::Arc::new(pantheon_runtime::swarm_exec::ScriptedWorker::new()),
            None,
        )),
    }
}

/// Write `bytes` as a stored upload and return its [`UploadInfo`].
fn store_upload(app: &App, name: &str, bytes: &[u8]) -> UploadInfo {
    let dir = app.data_dir.join("uploads");
    std::fs::create_dir_all(&dir).unwrap();
    let id = format!("upl_1_{:04}", rand_suffix());
    let path = dir.join(format!("{id}_{name}"));
    std::fs::write(&path, bytes).unwrap();
    UploadInfo {
        id,
        name: name.to_string(),
        mime: "application/octet-stream".to_string(),
        size_bytes: bytes.len() as u64,
        path,
    }
}

static SUFFIX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
fn rand_suffix() -> u32 {
    SUFFIX.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in entries {
        w.start_file(
            *name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

#[test]
fn zip_magic_bytes_detected() {
    let z = make_zip(&[("a.txt", b"hi")]);
    assert!(is_zip_bytes(&z));
    assert!(is_zip_bytes(b"PK\x03\x04rest"));
    assert!(is_zip_bytes(b"PK\x05\x06rest")); // empty archive
    assert!(is_zip_bytes(b"PK\x07\x08rest")); // spanned archive
    assert!(!is_zip_bytes(b""));
    assert!(!is_zip_bytes(b"PK\x03"));
    assert!(!is_zip_bytes(b"\x89PNG\r\n\x1a\n...."));
    assert!(!is_zip_bytes(b"just some text"));
}

#[test]
fn extract_round_trip_with_nested_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let app = test_app(dir.path());
    let bytes = make_zip(&[
        ("main.py", b"print('hi')"),
        ("data/notes.txt", b"hello"),
        ("data/deep/more.txt", b"deep"),
    ]);
    let info = store_upload(&app, "code.zip", &bytes);
    assert!(is_zip_upload(&info));

    let entries = extract_zip_upload(&app, &info).expect("extract");
    assert_eq!(entries.len(), 3);
    let root = zip_extract_dir(&app, &info.id);
    assert_eq!(std::fs::read(root.join("main.py")).unwrap(), b"print('hi')");
    assert_eq!(
        std::fs::read(root.join("data/deep/more.txt")).unwrap(),
        b"deep"
    );
    // Marker records the listing for idempotent reuse.
    assert!(root.join(".extracted.json").is_file());
}

#[test]
fn extract_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let app = test_app(dir.path());
    let info = store_upload(&app, "a.zip", &make_zip(&[("x.txt", b"1")]));
    let first = extract_zip_upload(&app, &info).unwrap();
    let second = extract_zip_upload(&app, &info).unwrap();
    assert_eq!(first.len(), second.len());
    assert_eq!(
        first[0].rel_path, second[0].rel_path,
        "repeat attach reuses the recorded listing"
    );
}

#[test]
fn zip_slip_is_rejected_and_writes_nothing_outside() {
    let dir = tempfile::tempdir().unwrap();
    let app = test_app(dir.path());
    let bytes = make_zip(&[("ok.txt", b"fine"), ("../evil.txt", b"pwned")]);
    let info = store_upload(&app, "evil.zip", &bytes);

    let err = extract_zip_upload(&app, &info).expect_err("zip-slip must fail closed");
    assert!(err.contains("unsafe entry path"), "got: {err}");
    assert!(
        !dir.path().join("evil.txt").exists(),
        "traversal entry must not escape the uploads dir"
    );
    // Partial extraction is cleaned up, not left half-written.
    assert!(
        !zip_extract_dir(&app, &info.id).exists()
            || !zip_extract_dir(&app, &info.id).join("ok.txt").exists(),
        "failed extraction leaves no committed tree"
    );
}

#[test]
fn absolute_entry_path_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = test_app(dir.path());
    let bytes = make_zip(&[("/abs.txt", b"nope")]);
    let info = store_upload(&app, "abs.zip", &bytes);
    let err = extract_zip_upload(&app, &info).expect_err("absolute path must fail closed");
    assert!(err.contains("unsafe entry path"), "got: {err}");
}

#[test]
fn entry_count_cap_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let app = test_app(dir.path());
    let many: Vec<(String, Vec<u8>)> = (0..=MAX_ZIP_ENTRIES)
        .map(|i| (format!("f{i}.txt"), b"x".to_vec()))
        .collect();
    let refs: Vec<(&str, &[u8])> = many
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    let info = store_upload(&app, "many.zip", &make_zip(&refs));
    let err = extract_zip_upload(&app, &info).expect_err("entry cap must fail closed");
    assert!(err.contains("entries"), "got: {err}");
}

/// Patch the first central-directory entry so it decodes as a symlink.
/// Real-world zips made with `zip --symlinks` carry such entries;
/// `SimpleFileOptions` cannot produce them because `unix_permissions`
/// masks off the file-type bits.
fn mark_first_entry_symlink(zip_bytes: &mut [u8]) {
    let sig = [0x50u8, 0x4b, 0x01, 0x02]; // central directory file header
    let start = zip_bytes
        .windows(4)
        .position(|w| w == sig)
        .expect("central directory");
    zip_bytes[start + 5] = 3; // version made by: host OS = Unix
    let attrs = 0o120777u32 << 16; // symlink type bits + rwxrwxrwx
    zip_bytes[start + 38..start + 42].copy_from_slice(&attrs.to_le_bytes());
}

#[test]
fn symlinks_are_skipped_never_materialized() {
    let dir = tempfile::tempdir().unwrap();
    let app = test_app(dir.path());
    let mut bytes = make_zip(&[("link", b"target.txt"), ("real.txt", b"real")]);
    assert!(
        !zip::ZipArchive::new(std::io::Cursor::new(&bytes))
            .unwrap()
            .by_name("link")
            .unwrap()
            .is_symlink(),
        "fixture sanity: unpatched entry is not a symlink"
    );
    mark_first_entry_symlink(&mut bytes);
    assert!(
        zip::ZipArchive::new(std::io::Cursor::new(&bytes))
            .unwrap()
            .by_name("link")
            .unwrap()
            .is_symlink(),
        "fixture sanity: patched entry must decode as a symlink"
    );

    let info = store_upload(&app, "sym.zip", &bytes);
    let entries = extract_zip_upload(&app, &info).expect("extract");
    assert_eq!(entries.len(), 1, "symlink entry is skipped");
    assert_eq!(entries[0].rel_path, PathBuf::from("real.txt"));
    assert!(
        !zip_extract_dir(&app, &info.id).join("link").exists(),
        "no symlink (or file) may materialize for the skipped entry"
    );
}

#[test]
fn non_zip_upload_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = test_app(dir.path());
    let info = store_upload(&app, "note.txt", b"just text");
    assert!(!is_zip_upload(&info));
    let err = extract_zip_upload(&app, &info).expect_err("non-zip must fail");
    assert!(err.contains("not a zip archive"), "got: {err}");
}

#[test]
fn corrupt_zip_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let app = test_app(dir.path());
    // Valid magic, garbage body.
    let info = store_upload(&app, "bad.zip", b"PK\x03\x04\x00garbage-not-a-zip");
    assert!(is_zip_upload(&info), "magic bytes still detect it");
    assert!(
        extract_zip_upload(&app, &info).is_err(),
        "corrupt archive must fail closed"
    );
}
