//! The model-run shell cannot read credentials or reach the network
//! unless asked (audit SEC-01, decision D1).
//!
//! The High sandbox bind-mounts the host root read-only, so before the
//! fix `cat ~/.pantheon/.env ~/.ssh/id_*` worked and the network was on.
//! These tests run the real shell profile through bwrap; on a host where
//! bwrap cannot create a user namespace (unprivileged user under
//! AppArmor's userns restriction, for example) the boundary fails closed
//! and the sandbox assertions are skipped with a message. CI runs this
//! file as root as well.

use pantheon_api::capability::{Capability, Decision, Policy};
use pantheon_exec::sandbox::run_sandboxed;
use pantheon_tools::builtins::{shell_network_requested, shell_profile, SHELL_NETWORK_CAP};
use std::path::PathBuf;

/// A fake home OUTSIDE /tmp: the sandbox mounts its own tmpfs on /tmp,
/// so files planted there would be invisible for the wrong reason. Under
/// root the target dir may sit in another user's 0750 home, which root
/// inside the sandbox's user namespace cannot traverse, so /srv is used.
fn fake_home() -> PathBuf {
    let root = std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(2) == Some("0"));
    let parent = if root {
        PathBuf::from("/srv")
    } else {
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
    };
    let base = parent.join(format!("sec01-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for (rel, body) in [
        (".pantheon/.env", "PANTHEON_KEY_OPENAI=sk-SEC01-DOTENV\n"),
        (".pantheon/logins.env", "SEC01-LOGINS\n"),
        (".pantheon/.secrets.key", "SEC01-VAULTKEY"),
        (".ssh/id_test", "SEC01-SSHKEY\n"),
        (".aws/credentials", "SEC01-AWS\n"),
        ("work/readme.txt", "VISIBLE-WORKSPACE\n"),
    ] {
        let p = base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    base
}

#[test]
fn shell_sandbox_hides_credentials_and_is_offline_by_default() {
    let home = fake_home();
    // Single test in this binary: mutating HOME / PANTHEON_DATA_DIR is safe.
    std::env::set_var("HOME", &home);
    std::env::remove_var("PANTHEON_DATA_DIR");
    let work = home.join("work");

    let profile = shell_profile(false);
    assert!(!profile.network, "shell must be offline unless asked");
    let hidden: Vec<&str> = profile.hidden_paths.iter().map(String::as_str).collect();
    let dotenv = home.join(".pantheon/.env");
    assert!(
        hidden
            .iter()
            .any(|h| *h == home.join(".pantheon").to_string_lossy()),
        "data dir must be hidden: {hidden:?}"
    );

    let script = format!(
        "cat {} {} {} {} {} 2>/dev/null; cat readme.txt",
        dotenv.display(),
        home.join(".pantheon/logins.env").display(),
        home.join(".pantheon/.secrets.key").display(),
        home.join(".ssh/id_test").display(),
        home.join(".aws/credentials").display(),
    );
    let r = match run_sandboxed(&profile, "sh", &["-c", &script], &work.to_string_lossy()) {
        Ok(r) => r,
        Err(e) if e.code == "SANDBOX_UNAVAILABLE" => {
            eprintln!("skip sandbox assertions: {}", e.cause);
            return;
        }
        Err(e) => panic!("{} {}", e.code, e.cause),
    };
    assert!(r.sandboxed);
    assert!(
        !r.output.contains("SEC01"),
        "credentials leaked into the sandbox: {}",
        r.output
    );
    assert!(
        r.output.contains("VISIBLE-WORKSPACE"),
        "the workspace must stay readable: {}",
        r.output
    );

    // Network: a listener on the host loopback is unreachable offline and
    // reachable once the call asked for (and was granted) network.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            drop(s);
        }
    });
    let probe = format!(
        "python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {port}), 2); print(\"CONNECTED\")' 2>&1"
    );
    let offline = run_sandboxed(&profile, "sh", &["-c", &probe], &work.to_string_lossy()).unwrap();
    // The Python traceback echoes the source line containing the marker, so
    // a substring check reports a failed connection as a success. Only a
    // standalone line is proof the probe actually connected.
    let connected = |output: &str| output.lines().any(|line| line.trim() == "CONNECTED");
    assert!(
        !connected(&offline.output),
        "offline shell reached the host: {}",
        offline.output
    );
    let online = run_sandboxed(
        &shell_profile(true),
        "sh",
        &["-c", &probe],
        &work.to_string_lossy(),
    )
    .unwrap();
    assert!(
        connected(&online.output),
        "network:true must reach the host: {}",
        online.output
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn network_is_an_explicit_approval_gated_request() {
    assert!(!shell_network_requested(
        &serde_json::json!({"command": "ls"})
    ));
    assert!(shell_network_requested(
        &serde_json::json!({"command": "cargo fetch", "network": true})
    ));
    let coder = Policy::coder();
    assert_eq!(
        coder.check(&Capability::Other(SHELL_NETWORK_CAP.into())),
        Decision::Approval,
        "the coder policy asks before a shell command goes online"
    );
    assert_eq!(
        Policy::researcher_readonly().check(&Capability::Other(SHELL_NETWORK_CAP.into())),
        Decision::Deny
    );
}
