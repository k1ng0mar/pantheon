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

    println!("· Downloading pantheon-helper {tag}");
    let (url, asset) = release_asset(&repo, &tag, "pantheon-helper");
    let bytes = match download_bytes(&url) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("update: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = verify_checksum(&url, &bytes) {
        eprintln!("update: {e}");
        std::process::exit(1);
    }
    println!("✓ Checksum verified");

    println!("· Downloading pantheon {tag}");
    let (main_url, main_asset) = release_asset(&repo, &tag, "pantheon");
    let main_bytes = match download_bytes(&main_url) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("update: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = verify_checksum(&main_url, &main_bytes) {
        eprintln!("update: {e}");
        std::process::exit(1);
    }
    println!("✓ Checksum verified");

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

    // Smoke-test the new main binary.
    println!("· Smoke-testing");
    match Command::new(&main_bin).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            println!("✓ New binary runs: {}", text.trim());
        }
        _ => {
            eprintln!("update: smoke test failed");
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

    // Backup current binary.
    let backup = current_exe.with_extension("prev");
    let _ = std::fs::copy(&current_exe, &backup);

    // Swap main binary via rename(2).
    println!("· Installing");
    let mut new_main = current_exe.clone();
    let mut new_name = current_exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pantheon")
        .to_string();
    new_name.push_str(".new");
    new_main.set_file_name(&new_name);
    if std::fs::copy(&main_bin, &new_main).is_err() {
        eprintln!("update: cannot write new binary");
        std::process::exit(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = std::fs::metadata(&new_main) {
            let mut perm = md.permissions();
            perm.set_mode(0o755);
            let _ = std::fs::set_permissions(&new_main, perm);
        }
    }
    if std::fs::rename(&new_main, &current_exe).is_err() {
        eprintln!("update: cannot swap binary");
        std::process::exit(1);
    }

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

    // Write stopped units to state file for rollback.
    if !stopped_units.is_empty() {
        let state_path = current_exe.with_extension("update-state");
        let _ = std::fs::write(&state_path, stopped_units.join("\n"));
    }

    // Restart systemd units.
    for unit in &stopped_units {
        restart_unit(unit);
    }
    if stopped_interactive {
        println!("  ⚠ interactive processes were stopped; restart them manually");
    }

    let _ = std::fs::remove_dir_all(&tmp);
    println!("✓ pantheon updated to {tag}");
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

    // Restart any systemd units that were stopped.
    let state_path = current_exe.with_extension("update-state");
    if state_path.is_file() {
        if let Ok(text) = std::fs::read_to_string(&state_path) {
            for unit in text.lines() {
                if !unit.trim().is_empty() {
                    restart_unit(unit.trim());
                }
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

fn verify_checksum(url: &str, bytes: &[u8]) -> Result<(), String> {
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
    let expected = text
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| c == '(' || c == ')' || c == '=' || c == '*')
                .to_lowercase()
        })
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_default();
    if expected.is_empty() {
        return Err("verify failed: no checksum".to_string());
    }
    let actual = hash_via_system(bytes)?;
    if actual != expected {
        return Err("verify failed: checksum mismatch".to_string());
    }
    Ok(())
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
    #[cfg(unix)]
    {
        let _ = Command::new("systemctl")
            .args(["--user", "start", unit])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("sc").args(["start", unit]).status();
    }
}
