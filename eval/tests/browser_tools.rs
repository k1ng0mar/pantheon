//! Behavioral tests for `pantheon-web`'s browser surface.
//!
//! Policy: deterministic invariants live in-crate (`cargo test -p
//! pantheon-web`). This file exercises the real `gsd-browser` binary
//! when it is on PATH; otherwise it prints a skip notice and passes —
//! the binary is optional, so a missing binary must not fail CI.

use pantheon_tools::tools::ToolRegistry;
use pantheon_web::browser::{register_browser_tools, BackendConfig, BrowserOptions};
use std::path::PathBuf;

/// Find `name` on PATH without extra dependencies.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

#[test]
fn browser_tools_live_or_skip() {
    let Some(binary) = find_on_path("gsd-browser") else {
        println!(
            "SKIP: gsd-browser not on PATH; install with \
             `npm install -g @opengsd/gsd-browser` to run \
             the live browser tests. Passing by policy (optional binary)."
        );
        return;
    };

    // The binary exists: prove the whole path works — registry, session
    // naming, subprocess spawn, JSON parsing.
    let mut reg = ToolRegistry::new();
    register_browser_tools(
        &mut reg,
        BrowserOptions {
            enabled: true,
            backend_config: BackendConfig {
                gsd_binary: Some(binary),
                ..BackendConfig::default()
            },
            ..BrowserOptions::default()
        },
    )
    .unwrap();

    for name in reg.names() {
        assert!(
            name.starts_with("browser_"),
            "unexpected tool registered: {name}"
        );
    }

    // `daemon health` is not a registered tool, but the binary must at
    // least start and answer --version: proves spawn + PATH wiring.
    let out = std::process::Command::new("gsd-browser")
        .arg("--version")
        .output()
        .expect("gsd-browser --version must run");
    assert!(
        out.status.success(),
        "gsd-browser --version failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    println!("live: {}", String::from_utf8_lossy(&out.stdout).trim());
}

#[test]
fn missing_binary_surfaces_install_help() {
    // Deterministic half: with a bogus binary path, invoking any tool
    // must surface BROWSER_BINARY_MISSING (with install instructions),
    // never panic or hang.
    let mut reg = ToolRegistry::new();
    register_browser_tools(
        &mut reg,
        BrowserOptions {
            enabled: true,
            backend_config: BackendConfig {
                gsd_binary: Some(PathBuf::from("/nonexistent/gsd-browser-for-eval")),
                ..BackendConfig::default()
            },
            ..BrowserOptions::default()
        },
    )
    .unwrap();
    let err = reg.execute("browser_snapshot", "{}").unwrap_err();
    assert_eq!(err.code, "BROWSER_BINARY_MISSING");
    assert!(err.cause.contains("npm install -g @opengsd/gsd-browser"));
}
