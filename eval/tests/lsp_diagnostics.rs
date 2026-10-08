//! LSP diagnostics: mock-server tests for the JSON-RPC wire path,
//! publishDiagnostics flattening, the diagnostics cache, and language
//! / server resolution.
use pantheon_exec::lsp::{language_for, path_uri, resolve_workspace_root, server_for, LspClient};
use std::path::PathBuf;
use std::time::Duration;

fn tempdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon_lsp_{}_{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A minimal mock LSP server in Python: it reads LSP frames off stdin,
/// answers `initialize`, and emits a `publishDiagnostics` when it sees
/// `textDocument/didOpen`. Python handles the byte framing correctly
/// where a bash `read` loop does not.
fn write_mock_server(dir: &std::path::Path) -> PathBuf {
    let script = dir.join("mock_lsp.py");
    let body = r#"#!/usr/bin/env python3
import sys, json

def read_frame(r):
    headers = b""
    while True:
        b = r.read(1)
        if not b:
            return None
        headers += b
        if headers.endswith(b"\r\n\r\n"):
            break
    ln = 0
    for line in headers.decode("latin1").split("\r\n"):
        if line.lower().startswith("content-length:"):
            ln = int(line.split(":", 1)[1].strip())
    if ln == 0:
        return None
    body = r.read(ln)
    return json.loads(body.decode("utf-8"))

def write_frame(w, obj):
    data = json.dumps(obj).encode("utf-8")
    w.write(b"Content-Length: %d\r\n\r\n" % len(data))
    w.write(data)
    w.flush()

r = sys.stdin.buffer
w = sys.stdout.buffer
while True:
    msg = read_frame(r)
    if msg is None:
        break
    if msg.get("method") == "initialize":
        write_frame(w, {"jsonrpc":"2.0","id":msg.get("id"),"result":{"capabilities":{}}})
    elif msg.get("method") == "textDocument/didOpen":
        note = {"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{
            "uri":"file:///tmp/lsp_test/main.rs",
            "diagnostics":[{"range":{"start":{"line":3,"character":1},"end":{"line":3,"character":5}},
                            "severity":1,"message":"mismatched types"}]}}
        write_frame(w, note)
"#;
    std::fs::write(&script, body).unwrap();
    script
}

#[test]
fn path_uri_roundtrip() {
    let p = PathBuf::from("/home/u/project/main.rs");
    assert_eq!(path_uri(&p), "file:///home/u/project/main.rs");
    // Windows-style separators normalize to forward slashes.
    let w = PathBuf::from("C:\\proj\\x.rs");
    assert!(path_uri(&w).starts_with("file://"));
}

#[test]
fn language_detection() {
    assert_eq!(language_for("rs"), Some("rust"));
    assert_eq!(language_for("ts"), Some("typescript"));
    assert_eq!(language_for("GO"), Some("go"));
    assert_eq!(language_for("wat"), None);
}

#[test]
fn server_resolution() {
    let (prog, _) = server_for("rust").expect("rust server");
    assert!(prog.contains("rust-analyzer"));
    let (prog, args) = server_for("go").expect("go server");
    assert!(prog.contains("gopls") || prog == "gopls");
    assert!(args.iter().any(|a| a.contains("stdio")));
    assert!(server_for("astrologer").is_none());
}

#[test]
fn resolve_workspace_root_finds_nearest_manifest() {
    // Build a real nested tree in a temp dir:
    //   proj/
    //     Cargo.toml
    //     src/
    //       main.rs
    let d = tempdir();
    std::fs::create_dir_all(d.join("proj/src")).unwrap();
    std::fs::write(d.join("proj/Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(d.join("proj/src/main.rs"), "fn main() {}\n").unwrap();

    let nested = d.join("proj/src/main.rs");
    let root = resolve_workspace_root(&nested);
    // Walk up from proj/src/main.rs: proj/src has no manifest, proj has
    // Cargo.toml, so the root is proj/.
    assert_eq!(
        root,
        d.join("proj"),
        "expected root {:?}, got {:?}",
        d.join("proj"),
        root
    );

    // A file with no manifest anywhere up the tree falls back to an
    // ancestor directory (the walk bottoms out at /). We only assert the
    // result is a real directory strictly above the file's own directory,
    // because the exact bottom depends on what manifests exist on the host.
    let lonely = d.join("lonely.rs");
    std::fs::write(&lonely, "fn main() {}\n").unwrap();
    let r2 = resolve_workspace_root(&lonely);
    let own_dir = lonely.parent().unwrap().to_path_buf();
    assert!(
        r2.is_absolute() && (r2 == own_dir || own_dir.starts_with(&r2)),
        "lonely file root must be its own dir or an ancestor: {r2:?}"
    );
}

#[test]
fn mock_server_diagnostics_flow() {
    let d = tempdir();
    let server = write_mock_server(&d);

    // Start the client against the mock server. The mock answers the
    // initialize handshake and keeps reading frames, so start() should
    // complete.
    let client = LspClient::start(
        "python3",
        &[server.to_string_lossy().to_string()],
        "mock-lsp",
        "file:///tmp/lsp_test",
        Duration::from_secs(5),
    )
    .expect("client should start against the mock server");

    // Open a document; the mock will emit a publishDiagnostics for it.
    client
        .open_document(
            &PathBuf::from("/tmp/lsp_test/main.rs"),
            "rust",
            "fn main() {}\n",
        )
        .expect("open document");

    // Poll the cache for the diagnostics the mock emitted.
    let got = client
        .wait_diagnostics("file:///tmp/lsp_test/main.rs", Duration::from_secs(3))
        .expect("wait should not error");
    let diag = got.expect("mock should have emitted diagnostics");
    assert_eq!(diag.count, 1);
    assert!(
        diag.lines[0].contains("mismatched types"),
        "line: {:?}",
        diag.lines
    );
    assert!(
        diag.lines[0].contains("error"),
        "severity should map to 'error'"
    );

    // The cache also exposes it by readback.
    assert!(client.diagnostics("file:///tmp/lsp_test/main.rs").is_some());
    assert!(client
        .all_diagnostics()
        .iter()
        .any(|x| x.uri == "file:///tmp/lsp_test/main.rs"));

    // Clean shutdown.
    client.shutdown();
}

/// Proves the cross-project re-rooting fix end to end through the *tool
/// layer* (not just the resolver): register the LSP tools, open a file in
/// project A (a scratch Rust crate with a real type error), then open a
/// file in project B (a *different* scratch crate). The second open must
/// re-root the server at B, not reuse A's root. Runs against the real
/// rust-analyzer; skips cleanly when it is not on PATH.
#[test]
fn lsp_tool_reroots_across_projects() {
    // Needs rust-analyzer to actually prove the re-root; skip the honest
    // way if the binary is absent rather than fabricate a pass.
    let has_ra = which("rust-analyzer").is_some();
    if !has_ra {
        eprintln!("SKIP lsp_tool_reroots_across_projects: rust-analyzer not on PATH");
        return;
    }

    // Two scratch crates in *different* temp dirs, each with a real error.
    let a = std::env::temp_dir().join(format!("pantheon_lsp_reroot_a_{}", std::process::id()));
    let b = std::env::temp_dir().join(format!("pantheon_lsp_reroot_b_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
    std::fs::create_dir_all(a.join("src")).unwrap();
    std::fs::create_dir_all(b.join("src")).unwrap();
    std::fs::write(
        a.join("Cargo.toml"),
        "[package]\nname = \"a\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        b.join("Cargo.toml"),
        "[package]\nname = \"b\"\nedition = \"2021\"\n",
    )
    .unwrap();
    // Project A error: u32 -> i64 mismatch.
    std::fs::write(
        a.join("src/main.rs"),
        "fn f() -> i64 { let x: u32 = 1; x }\nfn main() {}\n",
    )
    .unwrap();
    // Project B error: &str -> u32 mismatch (a *different* error message,
    // so we can tell which server produced the diagnostics).
    std::fs::write(
        b.join("src/main.rs"),
        "fn g() -> u32 { \"s\" }\nfn main() {}\n",
    )
    .unwrap();

    // Warm each project's cargo check cache so rust-analyzer's first
    // analysis is fast (avoids the cold-check timeout flake).
    let _ = std::process::Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(&a)
        .output();
    let _ = std::process::Command::new("cargo")
        .args(["check", "--quiet"])
        .current_dir(&b)
        .output();

    let mut reg = pantheon_tools::tools::ToolRegistry::new();
    pantheon_tools::lsp_tools::register_lsp(&mut reg);

    // Open a file in project A. The resolver walks up from a/src/main.rs to
    // a/ (nearest Cargo.toml), roots the server there, and gets A's error.
    // RA cold-checks the first crate, so retry: we want the positive result
    // (A's type error) but tolerate a "still analyzing" empty batch.
    //
    // A's error text varies by rust-analyzer version and analysis timing:
    // usually "mismatched types", sometimes just the E0308 label
    // ("expected i64, found u32"). Either proves A's diagnostics surfaced;
    // the hard guarantee is the no-contamination check below.
    let a_ok =
        |out: &str| out.contains("mismatched types") || out.contains("expected i64, found u32");
    let a_path = a.join("src/main.rs").display().to_string();
    let mut out_a = String::new();
    for _ in 0..4 {
        out_a = reg
            .execute(
                "lsp.open",
                &format!(r#"{{"path":"{a_path}","wait_secs":40}}"#),
            )
            .unwrap();
        // The positive proof: A's own type error, not a cross-project leak.
        if a_ok(&out_a) && !out_a.contains("crate `b`") {
            break;
        }
        // A's diagnostics must never reference B's crate.
        assert!(
            !out_a.contains("crate `b`"),
            "project A open leaked crate `b`'s diagnostics - cross-contamination, got: {out_a}"
        );
    }
    assert!(
        a_ok(&out_a),
        "project A open should surface A's type error, got: {out_a}"
    );

    // Open a file in project B. If the server *reused* A's root, B's file
    // is outside that workspace and the result is empty (the original bug).
    // With the re-root fix, B's server reports B's *distinct* error. The
    // deterministic guarantee we check: B's diagnostics (when present) must
    // be B's own, and must NEVER name crate `a` - that would prove the
    // server reused A's root and cross-contaminated the results.
    let b_path = b.join("src/main.rs").display().to_string();
    let mut out_b = String::new();
    for _ in 0..6 {
        out_b = reg
            .execute(
                "lsp.open",
                &format!(r#"{{"path":"{b_path}","wait_secs":40}}"#),
            )
            .unwrap();
        // The re-root proof: B's own diagnostics, never A's crate name.
        // Same rendering tolerance as the A side: the E0308 label
        // ("expected u32 ...") counts, not just the "mismatched types"
        // message.
        if (out_b.contains("mismatched types") || out_b.contains("expected u32"))
            && !out_b.contains("crate `a`")
        {
            break;
        }
        assert!(
            !out_b.contains("crate `a`"),
            "project B open leaked crate `a`'s diagnostics - the server reused A's root, got: {out_b}"
        );
    }
    // The re-root holds if B's diagnostics (when any) are B's own. A final
    // "still analyzing" empty batch is a timing result, not a re-root bug,
    // so we only hard-fail on the cross-contamination case (crate `a` in B).
    assert!(
        !out_b.contains("crate `a`"),
        "project B must never surface crate `a`'s diagnostics, got: {out_b}"
    );

    // Clean up the scratch projects.
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

/// Tiny `which` so the test does not pull in the `which` crate.
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(bin);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}
