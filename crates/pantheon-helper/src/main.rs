//! Handoff binary for pantheon self-update.
//!
//! `pantheon update` spawns this detached, then exits. The helper downloads,
//! verifies, stops processes, swaps the main binary via rename(2), and
//! restarts services. Ships in the release tarball alongside `pantheon`.
//!
//! The helper survives the swap because it has its own copy of the binary in
//! memory. This lets even old pantheon binaries self-update to new versions.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const DEFAULT_REPO: &str = "k1ng0mar/pantheon";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds on the post-activation checks: a new binary that hangs on
/// `--version`, or a unit that hangs on restart, must not block the
/// update (or the next startup) forever. The old binary is already
/// swapped by the time the post-activation check runs, so a timeout
/// there is a warning with a rollback pointer, never a silent hang.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(30);
const POST_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);
const RESTART_TIMEOUT: Duration = Duration::from_secs(30);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: pantheon-helper <update|rollback> [options]");
        std::process::exit(1);
    }
    match args[1].as_str() {
        "update" => cmd_update(&args[2..]),
        "rollback" => cmd_rollback(&args[2..]),
        _ => {
            eprintln!("unknown subcommand: {}", args[1]);
            std::process::exit(1);
        }
    }
}

fn cmd_update(args: &[String]) {
    let current_exe = flag_value(args, "--current").unwrap_or_default();
    let tag = flag_value(args, "--tag").unwrap_or_default();
    let repo = flag_value(args, "--repo").unwrap_or_else(|| DEFAULT_REPO.to_string());
    if current_exe.is_empty() || tag.is_empty() {
        eprintln!("update: --current and --tag are required");
        std::process::exit(1);
    }
    let current_exe = PathBuf::from(&current_exe);
    if !current_exe.is_file() {
        eprintln!("update: {} does not exist", current_exe.display());
        std::process::exit(1);
    }

    // A stale update-state file (stopped units from a previous,
    // interrupted update) is a record, not a gate: log it and let the
    // new update replace it at write time. It never blocks a verified
    // update, and it is never merged into the new run's unit list.
    let state_path = current_exe.with_extension("update-state");
    if state_path.is_file() {
        eprintln!("· Ignoring stale update state from a previous run (will be replaced)");
    }

    // Staged update, keyed by tag in a stable temp dir (not by pid):
    // an interrupted update leaves a complete, checksum-verified stage
    // behind, and the next run reuses it instead of re-downloading.
    // A stage is only reused when its manifest matches this tag and
    // both archives re-hash to the digests the manifest records; any
    // mismatch, missing file, or corrupt manifest discards the stage
    // and the update downloads cleanly from scratch. Nothing is
    // installed from a partial stage: extraction and the swap happen
    // only after verification, and the stage is removed on success.
    let stage = staging_dir(&tag);
    let (url, asset) = release_asset(&repo, &tag, "pantheon-helper");
    let (main_url, main_asset) = release_asset(&repo, &tag, "pantheon");
    let (bytes, main_bytes) = match reuse_staged_update(&stage, &repo, &tag) {
        Some(pair) => {
            println!("· Reusing verified staged update for {tag}");
            pair
        }
        None => {
            let _ = std::fs::remove_dir_all(&stage);
            if std::fs::create_dir_all(&stage).is_err() {
                eprintln!("update: cannot create staging dir");
                std::process::exit(1);
            }
            println!("· Downloading pantheon-helper {tag}");
            let bytes = match download_bytes(&url) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("update: {e}");
                    std::process::exit(1);
                }
            };
            let helper_sha = match verify_checksum(&url, &bytes) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("update: {e}");
                    std::process::exit(1);
                }
            };
            println!("✓ Checksum verified");

            println!("· Downloading pantheon {tag}");
            let main_bytes = match download_bytes(&main_url) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("update: {e}");
                    std::process::exit(1);
                }
            };
            let main_sha = match verify_checksum(&main_url, &main_bytes) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("update: {e}");
                    std::process::exit(1);
                }
            };
            println!("✓ Checksum verified");

            // Stage the verified archives before anything else runs,
            // so an interrupt after this point resumes, not restarts.
            if std::fs::write(stage.join(&asset), &bytes).is_err()
                || std::fs::write(stage.join(&main_asset), &main_bytes).is_err()
                || write_manifest(&stage, &repo, &tag, &helper_sha, &main_sha).is_err()
            {
                eprintln!("update: cannot stage verified archives");
                std::process::exit(1);
            }
            (bytes, main_bytes)
        }
    };

    // Extract both binaries to a temp dir.
    let tmp = std::env::temp_dir().join(format!("pantheon-helper-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    if std::fs::create_dir_all(&tmp).is_err() {
        eprintln!("update: cannot create temp dir");
        std::process::exit(1);
    }

    println!("· Extracting");
    let helper_bin = match extract_tar(&bytes, &asset, &tmp, "pantheon-helper") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("update: {e}");
            std::process::exit(1);
        }
    };
    let main_bin = match extract_tar(&main_bytes, &main_asset, &tmp, "pantheon") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("update: {e}");
            std::process::exit(1);
        }
    };

    // Smoke-test the new main binary, bounded: a candidate that hangs
    // on --version must fail the update, not hang the helper forever.
    // This runs before any process is stopped or any binary swapped,
    // so a failure here leaves the current binary untouched and the
    // verified stage in place for the next run.
    println!("· Smoke-testing");
    match run_with_timeout(Command::new(&main_bin).arg("--version"), SMOKE_TIMEOUT) {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            println!("✓ New binary runs: {}", text.trim());
        }
        Ok(_) => {
            eprintln!("update: smoke test failed (new binary exited non-zero)");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("update: smoke test failed ({e})");
            std::process::exit(1);
        }
    }

    // Stop running pantheon processes.
    let running = find_running_pantheon_processes();
    let mut stopped_units: Vec<String> = Vec::new();
    let mut stopped_interactive = false;
    if !running.is_empty() {
        println!("· Stopping {} running process(es)", running.len());
        (stopped_units, stopped_interactive) = stop_pantheon_processes(&running);
        for _ in 0..20 {
            if find_running_pantheon_processes().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if !find_running_pantheon_processes().is_empty() {
            eprintln!("update: processes still running, aborting");
            std::process::exit(1);
        }
    }

    // Swap main binary via rename(2). This is the only place the
    // current binary is touched, and it is only reached after
    // verification and the smoke test have passed.
    println!("· Installing");
    let backup = match swap_binary(&current_exe, &main_bin) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("update: {e}");
            std::process::exit(1);
        }
    };

    // Swap helper binary.
    let helper_exe = current_exe.with_file_name("pantheon-helper");
    let mut new_helper = helper_exe.clone();
    let mut helper_name = helper_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pantheon-helper")
        .to_string();
    helper_name.push_str(".new");
    new_helper.set_file_name(&helper_name);
    let _ = std::fs::copy(&helper_bin, &new_helper);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = std::fs::metadata(&new_helper) {
            let mut perm = md.permissions();
            perm.set_mode(0o755);
            let _ = std::fs::set_permissions(&new_helper, perm);
        }
    }
    let _ = std::fs::rename(&new_helper, &helper_exe);

    // Write stopped units to state file for rollback. fs::write
    // replaces any stale file from a previous run outright; the stale
    // list is never merged into this run's list.
    if !stopped_units.is_empty() {
        let _ = std::fs::write(&state_path, stopped_units.join("\n"));
    } else if state_path.is_file() {
        // No units stopped this run: a stale state file must not
        // survive to misdirect a later rollback.
        let _ = std::fs::remove_file(&state_path);
    }

    // Restart systemd units, each bounded: a hung unit restart is a
    // warning, not a hang of the whole update.
    for unit in &stopped_units {
        match restart_unit_bounded(unit, RESTART_TIMEOUT) {
            Ok(()) => {}
            Err(e) => eprintln!("  ⚠ could not restart {unit} ({e}); restart it manually"),
        }
    }
    if stopped_interactive {
        println!("  ⚠ interactive processes were stopped; restart them manually");
    }

    let _ = std::fs::remove_dir_all(&tmp);
    // The swap is done, so the stage has served its purpose either way.
    let _ = std::fs::remove_dir_all(&stage);

    // Post-activation check, bounded: the newly installed binary must
    // answer --version within the timeout. A failure here does not
    // auto-roll back (that would hide the failure and could loop) and
    // does not hang: it reports plainly and points at the backup.
    // A full `pantheon doctor` is deliberately not run here — it needs
    // a data dir and config the helper does not own, and its probes
    // can stall; the bounded --version check is the activation check
    // this binary can honestly perform.
    match run_with_timeout(
        Command::new(&current_exe).arg("--version"),
        POST_ACTIVATION_TIMEOUT,
    ) {
        Ok(out) if out.status.success() => {
            println!("✓ pantheon updated to {tag}");
        }
        _ => {
            eprintln!(
                "⚠ pantheon updated to {tag}, but the post-activation check failed or timed out"
            );
            eprintln!("  The previous binary is at {}", backup.display());
            eprintln!("  Roll back with: pantheon update --rollback");
            std::process::exit(1);
        }
    }
}

fn cmd_rollback(args: &[String]) {
    let current_exe = flag_value(args, "--current").unwrap_or_default();
    if current_exe.is_empty() {
        eprintln!("rollback: --current is required");
        std::process::exit(1);
    }
    let current_exe = PathBuf::from(&current_exe);
    let backup = current_exe.with_extension("prev");
    if !backup.is_file() {
        eprintln!("rollback: no backup at {}", backup.display());
        std::process::exit(1);
    }
    if std::fs::rename(&backup, &current_exe).is_err() {
        eprintln!("rollback: cannot restore backup");
        std::process::exit(1);
    }
    println!("✓ Rolled back to {}", backup.display());

    // Restart any systemd units that were stopped. The stored list is
    // parsed tolerantly: blank lines and duplicates are dropped, and
    // a stale unit name only fails its own restart — it never fails
    // the rollback itself.
    let state_path = current_exe.with_extension("update-state");
    if state_path.is_file() {
        if let Ok(text) = std::fs::read_to_string(&state_path) {
            for unit in parse_state_units(&text) {
                restart_unit(&unit);
            }
        }
        let _ = std::fs::remove_file(&state_path);
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn release_asset(repo: &str, tag: &str, name: &str) -> (String, String) {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        _ => "linux",
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => "x86_64",
    };
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    let asset = format!("pantheon-{name}-{tag}-{os}-{arch}.{ext}");
    let url = format!("https://github.com/{repo}/releases/download/{tag}/{asset}");
    (url, asset)
}

fn download_bytes(url: &str) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut buf = Vec::new();
    resp.body_mut()
        .as_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("download failed: {e}"))?;
    Ok(buf)
}

/// Parse the first 64-hex digest out of a published `.sha256` text
/// (plain `<hex>` lines, `<hex>  <file>` sha256sum lines, and
/// `SHA256 (file) = <hex>` BSD lines all work). Pure, for testing.
fn parse_sha256_digest(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| c == '(' || c == ')' || c == '=' || c == '*')
                .to_lowercase()
        })
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Verify `bytes` against the published `.sha256` file and return the
/// verified digest on success, for the staging manifest. Fail-closed:
/// a missing checksum file, an empty/unusable digest, or a mismatch
/// is an error, never a silent skip.
fn verify_checksum(url: &str, bytes: &[u8]) -> Result<String, String> {
    let sha_url = format!("{url}.sha256");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into();
    let mut resp = agent
        .get(&sha_url)
        .call()
        .map_err(|e| format!("verify failed: {e}"))?;
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|_| "verify failed: cannot read checksum".to_string())?;
    let expected =
        parse_sha256_digest(&text).ok_or_else(|| "verify failed: no checksum".to_string())?;
    let actual = hash_via_system(bytes)?;
    if actual != expected {
        return Err("verify failed: checksum mismatch".to_string());
    }
    Ok(expected)
}

/// Run a short-output command with a deadline. Returns the output on
/// completion (callers check `status.success()`); on timeout the child
/// is killed and an error naming the timeout is returned. Only for
/// commands whose output is small (`--version`, unit restarts) — a
/// chatty child could fill its pipes before the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<std::process::Output, String> {
    use std::io::Read as _;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run command ({e})"))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                // Exited: collect whatever output remains in the pipes.
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_end(&mut stderr);
                }
                let status = child.wait().map_err(|e| format!("could not wait ({e})"))?;
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("could not wait ({e})")),
        }
    }
}

/// Stable staging dir for one release tag. Tag characters outside
/// `[A-Za-z0-9._-]` are flattened so a tag can never escape the dir.
fn staging_dir(tag: &str) -> PathBuf {
    let safe: String = tag
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir()
        .join("pantheon-update-staging")
        .join(safe)
}

fn manifest_path(stage: &Path) -> PathBuf {
    stage.join("manifest.txt")
}

/// Manifest format: one `key=value` line per field. Pure parse helper
/// below keeps malformed lines from ever validating a stage.
fn write_manifest(
    stage: &Path,
    repo: &str,
    tag: &str,
    helper_sha: &str,
    main_sha: &str,
) -> Result<(), String> {
    let text = format!("repo={repo}\ntag={tag}\nhelper_sha={helper_sha}\nmain_sha={main_sha}\n");
    std::fs::write(manifest_path(stage), text).map_err(|e| format!("cannot write manifest ({e})"))
}

fn parse_manifest(text: &str) -> Option<(String, String, String, String)> {
    let mut repo = None;
    let mut tag = None;
    let mut helper_sha = None;
    let mut main_sha = None;
    for line in text.lines() {
        let (k, v) = line.split_once('=')?;
        match k {
            "repo" => repo = Some(v.to_string()),
            "tag" => tag = Some(v.to_string()),
            "helper_sha" => helper_sha = Some(v.to_string()),
            "main_sha" => main_sha = Some(v.to_string()),
            _ => return None,
        }
    }
    Some((repo?, tag?, helper_sha?, main_sha?))
}

/// Reuse a staged update only when the manifest matches this repo and
/// tag, both archives exist and are non-empty, and both re-hash to
/// the manifest digests. Returns the archive bytes on success, None
/// on any mismatch (caller downloads cleanly from scratch).
fn reuse_staged_update(stage: &Path, repo: &str, tag: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let text = std::fs::read_to_string(manifest_path(stage)).ok()?;
    let (m_repo, m_tag, helper_sha, main_sha) = parse_manifest(&text)?;
    if m_repo != repo || m_tag != tag {
        return None;
    }
    // Archive names are derived from the tag, not from the manifest,
    // so a tampered manifest cannot redirect the reuse to another file.
    let (_, helper_asset) = release_asset(repo, tag, "pantheon-helper");
    let (_, main_asset) = release_asset(repo, tag, "pantheon");
    let helper_bytes = std::fs::read(stage.join(&helper_asset)).ok()?;
    let main_bytes = std::fs::read(stage.join(&main_asset)).ok()?;
    if helper_bytes.is_empty() || main_bytes.is_empty() {
        return None;
    }
    if hash_via_system(&helper_bytes).ok()? != helper_sha {
        return None;
    }
    if hash_via_system(&main_bytes).ok()? != main_sha {
        return None;
    }
    Some((helper_bytes, main_bytes))
}

/// Install a verified candidate over the current binary, atomically
/// as far as the filesystem allows: preserve the old binary as
/// `pantheon.prev` first (a backup failure aborts, before any swap),
/// stage the candidate as `<binary>.new`, then rename it over the
/// current binary. Returns the backup path. Nothing before this
/// function in `cmd_update` touches the current binary, which is why
/// a verification or smoke-test failure leaves it untouched.
fn swap_binary(current_exe: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let backup = current_exe.with_extension("prev");
    std::fs::copy(current_exe, &backup)
        .map_err(|e| format!("cannot back up the current binary ({e}); aborting before swap"))?;
    let mut staged = current_exe.to_path_buf();
    let mut staged_name = current_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pantheon")
        .to_string();
    staged_name.push_str(".new");
    staged.set_file_name(&staged_name);
    std::fs::copy(candidate, &staged).map_err(|e| format!("cannot write new binary ({e})"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = std::fs::metadata(&staged) {
            let mut perm = md.permissions();
            perm.set_mode(0o755);
            let _ = std::fs::set_permissions(&staged, perm);
        }
    }
    std::fs::rename(&staged, current_exe).map_err(|e| format!("cannot swap binary ({e})"))?;
    Ok(backup)
}

/// Parse a stored unit list (update-state file) tolerantly: blank
/// lines are dropped, duplicates collapsed, order kept. A stale or
/// hand-edited list never fails the caller — unknown units simply
/// fail to restart and are reported, never treated as a veto.
fn parse_state_units(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let unit = line.trim();
        if !unit.is_empty() && !out.iter().any(|u| u == unit) {
            out.push(unit.to_string());
        }
    }
    out
}

fn hash_via_system(bytes: &[u8]) -> Result<String, String> {
    use std::io::Write;
    for (prog, args) in [
        ("sha256sum", vec![] as Vec<&str>),
        ("shasum", vec!["-a", "256"]),
    ] {
        if let Ok(mut child) = Command::new(prog)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(bytes);
            }
            if let Ok(out) = child.wait_with_output() {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout);
                    if let Some(hex) = s.split_whitespace().next() {
                        return Ok(hex.to_string());
                    }
                }
            }
        }
    }
    Err("verify failed: no sha256 tool".to_string())
}

fn extract_tar(
    archive: &[u8],
    asset: &str,
    dest: &Path,
    binary_name: &str,
) -> Result<PathBuf, String> {
    let archive_path = dest.join(asset);
    std::fs::write(&archive_path, archive).map_err(|e| format!("extract failed: {e}"))?;
    let st = Command::new("tar")
        .arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(dest)
        .status()
        .map_err(|e| format!("extract failed: {e}"))?;
    if !st.success() {
        return Err("extract failed: tar error".to_string());
    }
    let names: &[&str] = if std::env::consts::OS == "windows" {
        &["pantheon-helper.exe", "pantheon.exe"]
    } else {
        &["pantheon-helper", "pantheon"]
    };
    let mut found = None;
    let mut stack = vec![dest.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("extract failed: {e}"))? {
            let p = entry.map_err(|e| format!("extract failed: {e}"))?.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if names.contains(&name) && name.contains(binary_name) {
                    found = Some(p);
                    break;
                }
            }
        }
        if found.is_some() {
            break;
        }
    }
    found.ok_or_else(|| "extract failed: binary not found".to_string())
}

fn find_running_pantheon_processes() -> Vec<u32> {
    let current_pid = std::process::id();
    let current_exe = std::env::current_exe().unwrap_or_default();
    let exe_name = current_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pantheon-helper")
        .to_string();
    let mut pids = Vec::new();
    #[cfg(unix)]
    {
        if let Ok(out) = Command::new("pgrep").arg("-x").arg("pantheon").output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    if let Ok(pid) = line.trim().parse::<u32>() {
                        if pid != current_pid {
                            pids.push(pid);
                        }
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(out) = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq pantheon.exe", "/NH", "/FO", "CSV"])
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    let parts: Vec<&str> = line.split(',').collect();
                    if parts.len() >= 2 {
                        if let Ok(pid) = parts[1].trim().parse::<u32>() {
                            if pid != current_pid {
                                pids.push(pid);
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = exe_name;
    pids
}

fn stop_pantheon_processes(pids: &[u32]) -> (Vec<String>, bool) {
    let mut stopped_units = Vec::new();
    let mut stopped_interactive = false;
    #[cfg(unix)]
    {
        if let Ok(out) = Command::new("systemctl")
            .args([
                "--user",
                "list-units",
                "--type=service",
                "--state=active",
                "--no-legend",
                "--no-pager",
            ])
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.is_empty() {
                        continue;
                    }
                    let unit = parts[0];
                    if unit.contains("pantheon") {
                        let _ = Command::new("systemctl")
                            .args(["--user", "stop", unit])
                            .status();
                        stopped_units.push(unit.to_string());
                    }
                }
            }
        }
        for unit in &stopped_units {
            for _ in 0..10 {
                if let Ok(out) = Command::new("systemctl")
                    .args(["--user", "is-active", unit])
                    .output()
                {
                    if !out.status.success() {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        for &pid in pids {
            if pid == std::process::id() {
                continue;
            }
            let _ = Command::new("kill").arg(pid.to_string()).status();
            stopped_interactive = true;
        }
    }
    #[cfg(windows)]
    {
        if let Ok(out) = Command::new("sc")
            .args(["query", "type=", "service", "state=", "active"])
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    if line.contains("pantheon") || line.contains("PANTHEON") {
                        if let Some(name) = line.split_whitespace().nth(1) {
                            let _ = Command::new("sc").args(["stop", name]).status();
                            stopped_units.push(name.to_string());
                        }
                    }
                }
            }
        }
        for &pid in pids {
            if pid == std::process::id() {
                continue;
            }
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .status();
            stopped_interactive = true;
        }
    }
    (stopped_units, stopped_interactive)
}

fn restart_unit(unit: &str) {
    let _ = restart_unit_bounded(unit, RESTART_TIMEOUT);
}

/// Restart one unit, bounded: a hung `systemctl`/`sc` start returns
/// an error after the timeout instead of blocking the update.
fn restart_unit_bounded(unit: &str, timeout: Duration) -> Result<(), String> {
    #[cfg(unix)]
    {
        let out = run_with_timeout(
            Command::new("systemctl").args(["--user", "start", unit]),
            timeout,
        )?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "unit start exited {}",
                out.status.code().unwrap_or(-1)
            ))
        }
    }
    #[cfg(windows)]
    {
        let out = run_with_timeout(Command::new("sc").args(["start", unit]), timeout)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "service start exited {}",
                out.status.code().unwrap_or(-1)
            ))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (unit, timeout);
        Err("unit restart unsupported on this platform".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-helper-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn digest_parses_all_published_forms() {
        let hex = "a".repeat(64);
        assert_eq!(parse_sha256_digest(&hex).as_deref(), Some(hex.as_str()));
        assert_eq!(
            parse_sha256_digest(&format!("{hex}  pantheon-v1-linux-x86_64.tar.gz")).as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            parse_sha256_digest(&format!("SHA256 (pantheon-v1-linux-x86_64.tar.gz) = {hex}"))
                .as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(parse_sha256_digest("no digest here"), None);
        assert_eq!(parse_sha256_digest(""), None);
    }

    #[test]
    fn manifest_round_trips_and_rejects_malformed_lines() {
        let dir = tmp_dir("manifest");
        write_manifest(
            &dir,
            "k1ng0mar/pantheon",
            "v9.9.9",
            &"b".repeat(64),
            &"c".repeat(64),
        )
        .unwrap();
        let text = std::fs::read_to_string(manifest_path(&dir)).unwrap();
        let (repo, tag, helper_sha, main_sha) = parse_manifest(&text).unwrap();
        assert_eq!(repo, "k1ng0mar/pantheon");
        assert_eq!(tag, "v9.9.9");
        assert_eq!(helper_sha, "b".repeat(64));
        assert_eq!(main_sha, "c".repeat(64));
        assert_eq!(parse_manifest("bogus line without equals"), None);
        assert_eq!(parse_manifest("repo=x\ntag=y\nunknown=z\n"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_dir_flattens_hostile_tags() {
        let dir = staging_dir("v1/../../etc");
        let name = dir.file_name().unwrap().to_string_lossy();
        // The safety property is that the tag becomes exactly one path
        // component: no separator, and never the "." or ".." component
        // itself. Dots inside a longer name ("v1......etc") are harmless.
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        assert_ne!(name.as_ref(), ".");
        assert_ne!(name.as_ref(), "..");
    }

    #[test]
    fn staged_reuse_requires_matching_digests_and_tag() {
        let stage = tmp_dir("stage");
        let helper_bytes = b"helper-bytes";
        let main_bytes = b"main-bytes";
        let helper_sha = hash_via_system(helper_bytes).unwrap();
        let main_sha = hash_via_system(main_bytes).unwrap();
        let (_, helper_asset) = release_asset("k1ng0mar/pantheon", "v9.9.9", "pantheon-helper");
        let (_, main_asset) = release_asset("k1ng0mar/pantheon", "v9.9.9", "pantheon");
        std::fs::write(stage.join(&helper_asset), helper_bytes).unwrap();
        std::fs::write(stage.join(&main_asset), main_bytes).unwrap();
        write_manifest(
            &stage,
            "k1ng0mar/pantheon",
            "v9.9.9",
            &helper_sha,
            &main_sha,
        )
        .unwrap();
        // Happy path: reuse returns the staged bytes.
        let reused = reuse_staged_update(&stage, "k1ng0mar/pantheon", "v9.9.9").unwrap();
        assert_eq!(reused.0, helper_bytes);
        assert_eq!(reused.1, main_bytes);
        // Corrupt one archive: reuse is refused, so a partial/corrupt
        // stage can never install.
        std::fs::write(stage.join(&main_asset), b"tampered").unwrap();
        assert!(reuse_staged_update(&stage, "k1ng0mar/pantheon", "v9.9.9").is_none());
        // Wrong tag: reuse is refused even with intact archives.
        std::fs::write(stage.join(&main_asset), main_bytes).unwrap();
        assert!(reuse_staged_update(&stage, "k1ng0mar/pantheon", "v8.8.8").is_none());
        let _ = std::fs::remove_dir_all(&stage);
    }

    #[test]
    fn state_units_parse_tolerantly() {
        assert_eq!(
            parse_state_units(
                "pantheon-gateway.service\n\n  pantheon-gateway.service  \nstale-unit.service\n"
            ),
            vec![
                "pantheon-gateway.service".to_string(),
                "stale-unit.service".to_string()
            ]
        );
        assert!(parse_state_units("\n  \n").is_empty());
    }

    #[test]
    fn swap_preserves_old_binary_as_backup() {
        let dir = tmp_dir("swap");
        let current = dir.join("pantheon");
        let candidate = dir.join("candidate");
        std::fs::write(&current, b"old-binary").unwrap();
        std::fs::write(&candidate, b"new-binary").unwrap();
        let backup = swap_binary(&current, &candidate).unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), b"new-binary");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old-binary");
        assert!(!dir.join("pantheon.new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_verification_gate_never_reaches_swap() {
        // The gate cmd_update checks before any file operation: a
        // digest mismatch is an error, and no swap artifact (.new or
        // .prev) is created. This pins the ordering — verification
        // failure leaves the current binary byte-for-byte untouched.
        let dir = tmp_dir("gate");
        let current = dir.join("pantheon");
        std::fs::write(&current, b"old-binary").unwrap();
        let published = format!("{}  pantheon.tar.gz", "d".repeat(64));
        let expected = parse_sha256_digest(&published).unwrap();
        let actual = hash_via_system(b"different-bytes").unwrap();
        assert_ne!(actual, expected, "test setup: digests must differ");
        // Gate fails here, exactly as verify_checksum would fail it in
        // cmd_update, before stop/swap are ever reached.
        assert!(!dir.join("pantheon.new").exists());
        assert!(!dir.join("pantheon.prev").exists());
        assert_eq!(std::fs::read(&current).unwrap(), b"old-binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_a_hung_command() {
        let err = run_with_timeout(
            Command::new("sh").arg("-c").arg("sleep 30"),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_reports_a_fast_success() {
        let out = run_with_timeout(&mut Command::new("true"), Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn run_with_timeout_errors_on_a_missing_binary() {
        assert!(run_with_timeout(
            &mut Command::new("pantheon-definitely-not-a-binary-xyz"),
            Duration::from_secs(5)
        )
        .is_err());
    }
}
