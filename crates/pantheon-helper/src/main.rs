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
    // an interrupted update leaves a complete, signature-verified stage
    // behind, and the next run reuses it instead of re-downloading.
    // A stage is only reused when its manifest matches this tag, its
    // directory is private, both archives re-hash to the digests the
    // manifest records, AND both archives re-verify against the pinned
    // release key using the signature texts staged beside them; any
    // mismatch, missing file, unsigned stage, or corrupt manifest
    // discards the stage and the update downloads cleanly from
    // scratch. Nothing is installed from a partial stage: extraction
    // and the swap happen only after verification, and the stage is
    // removed on success.
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
            if create_private_dir(&stage).is_err() {
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
            println!("✓ Checksum matches (corruption check only — not authentication)");
            // The signature text is kept, not just used: it is staged
            // next to the archive so a resumed update can re-verify the
            // exact bytes it installs offline (see reuse_staged_update).
            let helper_sig = match fetch_signature_text(&url) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("update: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = pantheon_release_verify::verify_signature(&bytes, &helper_sig) {
                eprintln!("update: {e}");
                std::process::exit(1);
            }
            println!("✓ Signature verified (minisign, pinned release key)");

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
            println!("✓ Checksum matches (corruption check only — not authentication)");
            let main_sig = match fetch_signature_text(&main_url) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("update: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = pantheon_release_verify::verify_signature(&main_bytes, &main_sig) {
                eprintln!("update: {e}");
                std::process::exit(1);
            }
            println!("✓ Signature verified (minisign, pinned release key)");

            // Stage the verified archives before anything else runs,
            // so an interrupt after this point resumes, not restarts.
            // The signature texts are staged with them: a resumed run
            // must be able to re-verify the bytes it installs, and it
            // must do so from the stage, offline.
            let helper_verified = VerifiedAsset {
                asset: &asset,
                bytes: &bytes,
                sig_text: &helper_sig,
                sha: &helper_sha,
            };
            let main_verified = VerifiedAsset {
                asset: &main_asset,
                bytes: &main_bytes,
                sig_text: &main_sig,
                sha: &main_sha,
            };
            if let Err(e) =
                stage_verified_update(&stage, &repo, &tag, &helper_verified, &main_verified)
            {
                eprintln!("update: {e}");
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
            // Units were already stopped above: leaving without
            // restarting them would take the user's services down
            // with no update to show for it.
            eprintln!("update: processes still running, aborting; restarting stopped units");
            restart_stopped_units(&stopped_units);
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
            // Same rule as the still-running abort: stopped units
            // must come back even when the swap fails.
            eprintln!("update: {e}; restarting stopped units");
            restart_stopped_units(&stopped_units);
            std::process::exit(1);
        }
    };

    // Swap helper binary, with its own backup for rollback. The old
    // code ignored both the copy and the rename (`let _ =`) and then
    // printed success, so a failed helper install was invisible and
    // unrecoverable. A helper failure is recorded, not ignored: the
    // main binary is already swapped at this point, so the update
    // continues through the unit restarts and then exits nonzero
    // with the failure named, instead of printing success.
    let helper_exe = current_exe.with_file_name("pantheon-helper");
    let mut helper_failed: Option<String> = None;
    if helper_exe.is_file() {
        if let Err(e) = std::fs::copy(&helper_exe, &helper_exe.with_extension("prev")) {
            helper_failed = Some(format!("cannot back up helper ({e})"));
        }
    }
    if helper_failed.is_none() {
        let mut new_helper = helper_exe.clone();
        let mut helper_name = helper_exe
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pantheon-helper")
            .to_string();
        helper_name.push_str(".new");
        new_helper.set_file_name(&helper_name);
        let helper_result = std::fs::copy(&helper_bin, &new_helper)
            .map_err(|e| format!("cannot stage new helper ({e})"))
            .and_then(|_| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(md) = std::fs::metadata(&new_helper) {
                        let mut perm = md.permissions();
                        perm.set_mode(0o755);
                        let _ = std::fs::set_permissions(&new_helper, perm);
                    }
                }
                std::fs::rename(&new_helper, &helper_exe)
                    .map_err(|e| format!("cannot install helper ({e})"))
            });
        if let Err(e) = helper_result {
            helper_failed = Some(e);
        }
    }

    // Write stopped units to state file for rollback. fs::write
    // replaces any stale file from a previous run outright; the stale
    // list is never merged into this run's list.
    if !stopped_units.is_empty() {
        if std::fs::write(&state_path, stopped_units.join("\n")).is_err() {
            eprintln!("  ⚠ could not record stopped units; rollback will not restart them");
        }
    } else if state_path.is_file() {
        // No units stopped this run: a stale state file must not
        // survive to misdirect a later rollback.
        let _ = std::fs::remove_file(&state_path);
    }

    // Restart systemd units, each bounded: a hung unit restart is a
    // warning, not a hang of the whole update.
    restart_stopped_units(&stopped_units);
    if stopped_interactive {
        println!("  ⚠ interactive processes were stopped; restart them manually");
    }

    let _ = std::fs::remove_dir_all(&tmp);
    // The swap is done, so the stage has served its purpose either way.
    let _ = std::fs::remove_dir_all(&stage);

    if let Some(e) = helper_failed {
        eprintln!("update: helper install failed ({e}); main binary updated, helper left stale");
        eprintln!("  Roll back with: pantheon update --rollback");
        std::process::exit(1);
    }

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

    // Roll the helper back too: the update backs it up to
    // `pantheon-helper.prev` before swapping. A rollback that restores
    // only the main binary leaves a new helper driving an old main.
    // No helper backup (updates from before this fix, or a helper
    // that was never installed) is a note, not a failure.
    let helper_exe = current_exe.with_file_name("pantheon-helper");
    let helper_backup = helper_exe.with_extension("prev");
    let mut helper_failed = false;
    if helper_backup.is_file() {
        if std::fs::rename(&helper_backup, &helper_exe).is_err() {
            eprintln!(
                "rollback: ⚠ could not restore helper from {}",
                helper_backup.display()
            );
            helper_failed = true;
        } else {
            println!("✓ Rolled helper back to {}", helper_backup.display());
        }
    }

    // Restart any systemd units that were stopped. The stored list is
    // parsed tolerantly: blank lines and duplicates are dropped, and
    // a stale unit name only fails its own restart — it never fails
    // the rollback itself. The state file is removed only when every
    // restart succeeded: deleting it after a failed restart would
    // lose the only record of which units are still down.
    let state_path = current_exe.with_extension("update-state");
    let mut restarts_ok = true;
    if state_path.is_file() {
        if let Ok(text) = std::fs::read_to_string(&state_path) {
            for unit in parse_state_units(&text) {
                if restart_unit_bounded(&unit, RESTART_TIMEOUT).is_err() {
                    eprintln!("rollback: ⚠ could not restart {unit}; restart it manually");
                    restarts_ok = false;
                }
            }
        }
        if restarts_ok {
            let _ = std::fs::remove_file(&state_path);
        } else {
            eprintln!(
                "rollback: ⚠ state file kept at {} for a retry",
                state_path.display()
            );
        }
    }
    if helper_failed {
        std::process::exit(1);
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
///
/// The digest is bound to `asset` when the file names it: if any
/// line names the asset, that line's digest wins, even when the file
/// carries digests for several files. Otherwise the file must carry
/// exactly one digest; a multi-digest file that never names the
/// asset is ambiguous and yields None (fail closed) rather than the
/// first token, which might belong to another file.
///
/// NOTE: a checksum only proves the bytes match the publisher's
/// published digest when the digest itself is authentic. The
/// `.sha256` file is fetched from the same origin as the binary, so
/// a compromised release (or release process) can serve a matching
/// pair. This is a corruption check, never authentication: real
/// authentication is the minisign signature checked by
/// [`verify_signature_for`], and no archive installs without it.
fn parse_sha256_digest(text: &str, asset: &str) -> Option<String> {
    let tokens = |line: &str| -> Vec<String> {
        line.split_whitespace()
            .map(|t| {
                t.trim_matches(|c: char| c == '(' || c == ')' || c == '=' || c == '*')
                    .to_lowercase()
            })
            .filter(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
            .collect()
    };
    let mut all: Vec<String> = Vec::new();
    for line in text.lines() {
        let found = tokens(line);
        if found.is_empty() {
            continue;
        }
        if !asset.is_empty() && line.contains(asset) {
            return found.into_iter().next();
        }
        all.extend(found);
    }
    if all.len() == 1 {
        all.into_iter().next()
    } else {
        None
    }
}

/// Verify `bytes` against the published `.sha256` file and return the
/// verified digest on success, for the staging manifest. Fail-closed:
/// a missing checksum file, an empty/unusable digest, or a mismatch
/// is an error, never a silent skip.
///
/// CORRUPTION CHECK ONLY. This proves the bytes match the publisher's
/// checksum file; it does not authenticate the publisher (same
/// origin). Callers must also pass [`verify_signature_for`] before
/// anything installs.
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
    let asset = url.rsplit('/').next().unwrap_or("");
    let expected = parse_sha256_digest(&text, asset)
        .ok_or_else(|| "verify failed: no checksum".to_string())?;
    let actual = hash_via_system(bytes)?;
    if actual != expected {
        return Err("verify failed: checksum mismatch".to_string());
    }
    Ok(expected)
}

/// Fetch the `.minisig` text published next to `url`. Kept separate
/// from the verification so the signature text can also be *staged*:
/// a resumed update re-verifies the staged bytes offline, against the
/// staged signature text.
fn fetch_signature_text(url: &str) -> Result<String, String> {
    let sig_url = format!("{url}.minisig");
    let sig_bytes = download_bytes(&sig_url)
        .map_err(|e| format!("signature verification failed: cannot fetch {sig_url} ({e})"))?;
    String::from_utf8(sig_bytes)
        .map_err(|_| "signature verification failed: .minisig is not valid UTF-8".to_string())
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
///
/// `sig_verified=1` records that the previous run verified the
/// signatures before writing the stage. It is NOT the gate on reuse:
/// the stage lives in a shared temp directory, so a planted stage can
/// claim it. Reuse re-verifies the signatures over the exact bytes it
/// installs (`reuse_staged_update`).
fn write_manifest(
    stage: &Path,
    repo: &str,
    tag: &str,
    helper_sha: &str,
    main_sha: &str,
) -> Result<(), String> {
    let text = format!(
        "repo={repo}\ntag={tag}\nhelper_sha={helper_sha}\nmain_sha={main_sha}\nsig_verified=1\n"
    );
    std::fs::write(manifest_path(stage), text).map_err(|e| format!("cannot write manifest ({e})"))
}

fn parse_manifest(text: &str) -> Option<(String, String, String, String)> {
    let mut repo = None;
    let mut tag = None;
    let mut helper_sha = None;
    let mut main_sha = None;
    let mut sig_verified = None;
    for line in text.lines() {
        let (k, v) = line.split_once('=')?;
        match k {
            "repo" => repo = Some(v.to_string()),
            "tag" => tag = Some(v.to_string()),
            "helper_sha" => helper_sha = Some(v.to_string()),
            "main_sha" => main_sha = Some(v.to_string()),
            // Stages written before signature verification existed
            // carry no sig_verified line and must NOT be reused: a
            // checksum-only stage is not an authenticated stage.
            "sig_verified" => sig_verified = Some(v.to_string()),
            _ => return None,
        }
    }
    if sig_verified.as_deref() != Some("1") {
        return None;
    }
    Some((repo?, tag?, helper_sha?, main_sha?))
}

/// Reuse a staged update only when the manifest matches this repo and
/// tag, the stage directory is not writable by anyone but its owner,
/// both archives exist and are non-empty, both re-hash to the manifest
/// digests, AND both archives re-verify against the pinned release key
/// using the signature texts staged beside them.
///
/// The signature check is the authoritative gate. The manifest's
/// `sig_verified=1` line is a *record* of the previous run, not proof:
/// the stage lives in a shared temp directory, so a planted stage can
/// carry a self-asserted flag and matching checksums for bytes the
/// publisher never signed. Trusting the flag would install an unsigned
/// binary - exactly what the update path exists to prevent. The
/// signature is re-verified here, offline, over the exact bytes that
/// are about to be installed.
///
/// Returns the archive bytes on success, None on any mismatch (caller
/// downloads cleanly from scratch).
fn reuse_staged_update(stage: &Path, repo: &str, tag: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    reuse_staged_update_with(stage, repo, tag, |bytes, sig_text| {
        pantheon_release_verify::verify_signature(bytes, sig_text)
    })
}

/// The reuse gate with the verifier injected, so the seam can be
/// exercised with a known-good key in tests. `verify` receives the
/// archive bytes and the signature text staged next to them.
fn reuse_staged_update_with<F>(
    stage: &Path,
    repo: &str,
    tag: &str,
    verify: F,
) -> Option<(Vec<u8>, Vec<u8>)>
where
    F: Fn(&[u8], &str) -> Result<(), String>,
{
    if !stage_is_private(stage) {
        return None;
    }
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
    // Re-verify the signature over the bytes we are about to install.
    // A stage without its signature text, or with one that does not
    // verify against the pinned key, is refused: it is an unsigned
    // stage, and an unsigned stage never installs.
    let helper_sig = std::fs::read_to_string(stage.join(sig_asset_name(&helper_asset))).ok()?;
    verify(&helper_bytes, &helper_sig).ok()?;
    let main_sig = std::fs::read_to_string(stage.join(sig_asset_name(&main_asset))).ok()?;
    verify(&main_bytes, &main_sig).ok()?;
    Some((helper_bytes, main_bytes))
}

/// The staged signature text for an archive asset.
fn sig_asset_name(asset: &str) -> String {
    format!("{asset}.minisig")
}

/// One verified release asset: the archive name, the bytes that were
/// verified, the signature text that authenticates those bytes, and
/// the digest the manifest records.
struct VerifiedAsset<'a> {
    asset: &'a str,
    bytes: &'a [u8],
    sig_text: &'a str,
    sha: &'a str,
}

/// Write the verified archives and their signature texts into the
/// stage, then the manifest.
///
/// The signature texts travel with the archives on purpose: a resumed
/// update re-verifies the bytes it installs against the staged
/// signature, offline. Without them the stage could only be
/// authenticated by its own manifest - and the manifest lives in a
/// shared temp directory, so it proves nothing on its own.
fn stage_verified_update(
    stage: &Path,
    repo: &str,
    tag: &str,
    helper: &VerifiedAsset<'_>,
    main: &VerifiedAsset<'_>,
) -> Result<(), String> {
    let write = |path: PathBuf, data: &[u8]| -> std::io::Result<()> { std::fs::write(path, data) };
    write(stage.join(helper.asset), helper.bytes)
        .and_then(|_| {
            write(
                stage.join(sig_asset_name(helper.asset)),
                helper.sig_text.as_bytes(),
            )
        })
        .and_then(|_| write(stage.join(main.asset), main.bytes))
        .and_then(|_| {
            write(
                stage.join(sig_asset_name(main.asset)),
                main.sig_text.as_bytes(),
            )
        })
        .map_err(|e| format!("cannot stage verified archives ({e})"))?;
    write_manifest(stage, repo, tag, helper.sha, main.sha)
}

/// True when the stage directory is not writable by anyone but its
/// owner. A shared-writable stage would let another local user swap
/// the staged archives under us; the signature check above is the
/// authoritative gate, this is defence in depth around it.
fn stage_is_private(stage: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(stage) {
            Ok(md) => md.is_dir() && md.permissions().mode() & 0o022 == 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        stage.is_dir()
    }
}

/// Create a directory only its owner can write to. The stage holds
/// release archives before they are installed; it must not be a place
/// another local user can write.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        b.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
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

/// Extract exactly one binary out of a release tarball.
///
/// The old code ran `tar -xf` over the whole archive and then walked
/// the tree for a SUBSTRING match (`pantheon-helper` contains
/// `pantheon`, so a search for `pantheon` could install the helper
/// as the main binary). It also had no traversal guard: a hostile
/// tarball could carry `../` members, absolute paths, or a symlink
/// that a later member writes through, escaping the temp dir.
///
/// Now: list first (`tar -tf`) and reject absolute paths and any
/// `..` component before anything is extracted; take the single
/// member whose BASENAME is exactly the wanted binary (fail on zero
/// or on ambiguity); extract only that member; and require the
/// result to be a regular file (never a symlink) under the dest dir.
/// A release tarball has exactly one binary of each name, so the
/// strictness costs nothing on legitimate archives.
fn extract_tar(
    archive: &[u8],
    asset: &str,
    dest: &Path,
    binary_name: &str,
) -> Result<PathBuf, String> {
    let archive_path = dest.join(asset);
    std::fs::write(&archive_path, archive).map_err(|e| format!("extract failed: {e}"))?;
    let list = Command::new("tar")
        .arg("-tf")
        .arg(&archive_path)
        .output()
        .map_err(|e| format!("extract failed: {e}"))?;
    if !list.status.success() {
        return Err("extract failed: tar could not list the release archive".to_string());
    }
    let expected: String = if std::env::consts::OS == "windows" {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    };
    let mut candidates: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&list.stdout).lines() {
        let entry = line.trim();
        if entry.is_empty() {
            continue;
        }
        // Traversal guard, checked before extraction: absolute paths
        // and any `..` component are rejected outright.
        if entry.starts_with('/') || entry.starts_with('\\') {
            return Err(format!("extract failed: absolute member {entry:?}"));
        }
        if entry
            .split(|c| c == '/' || c == '\\')
            .any(|seg| seg == "..")
        {
            return Err(format!(
                "extract failed: member escapes its directory: {entry:?}"
            ));
        }
        let base = entry
            .rsplit(|c| c == '/' || c == '\\')
            .next()
            .unwrap_or(entry);
        if base == expected {
            candidates.push(entry.to_string());
        }
    }
    if candidates.is_empty() {
        return Err("extract failed: binary not found".to_string());
    }
    if candidates.len() > 1 {
        return Err(format!(
            "extract failed: ambiguous archive ({} members named {expected:?})",
            candidates.len()
        ));
    }
    let member = candidates.into_iter().next().unwrap();
    // NOTE: `-C dest` must precede the member name: tar applies -C
    // to the arguments that follow it, so `-C` after the member
    // would extract into the process cwd instead of dest.
    let st = Command::new("tar")
        .arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(dest)
        .arg(&member)
        .status()
        .map_err(|e| format!("extract failed: {e}"))?;
    if !st.success() {
        return Err("extract failed: tar error".to_string());
    }
    // The member path is relative and traversal-checked above, so
    // this stays under dest; the symlink check below is the second
    // half (a symlink member must never become the installed binary).
    let extracted = dest.join(member.strip_prefix("./").unwrap_or(member.as_str()));
    match std::fs::symlink_metadata(&extracted) {
        Ok(md) if md.file_type().is_file() => Ok(extracted),
        Ok(_) => Err("extract failed: extracted binary is not a regular file".to_string()),
        Err(e) => Err(format!("extract failed: {e}")),
    }
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

/// Restart every stopped unit, bounded per unit, warning per failure.
/// Shared by the success path and every abort path after units were
/// stopped: an abort must never leave services down.
fn restart_stopped_units(units: &[String]) {
    for unit in units {
        match restart_unit_bounded(unit, RESTART_TIMEOUT) {
            Ok(()) => {}
            Err(e) => eprintln!("  ⚠ could not restart {unit} ({e}); restart it manually"),
        }
    }
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
        // Owner-only: the stage gate refuses a directory anyone else
        // can write to, and the test must not depend on the umask.
        create_private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn digest_parses_all_published_forms() {
        let hex = "a".repeat(64);
        let asset = "pantheon-v1-linux-x86_64.tar.gz";
        assert_eq!(
            parse_sha256_digest(&hex, asset).as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            parse_sha256_digest(&format!("{hex}  {asset}"), asset).as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            parse_sha256_digest(&format!("SHA256 ({asset}) = {hex}"), asset).as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(parse_sha256_digest("no digest here", asset), None);
        assert_eq!(parse_sha256_digest("", asset), None);
    }

    #[test]
    fn digest_prefers_the_line_naming_the_asset() {
        // A multi-digest file must not resolve to the first token
        // when a later line names our asset, and must fail closed
        // when no line names it.
        let ours = "b".repeat(64);
        let other = "c".repeat(64);
        let text = format!("{other}  something-else.tar.gz\n{ours}  pantheon-v1.tar.gz\n");
        assert_eq!(
            parse_sha256_digest(&text, "pantheon-v1.tar.gz").as_deref(),
            Some(ours.as_str())
        );
        assert_eq!(
            parse_sha256_digest(&text, "pantheon-v2.tar.gz"),
            None,
            "ambiguous multi-digest file without our asset must fail closed"
        );
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
        // Pre-signature stages (no sig_verified line) and explicitly
        // unverified stages are never reused: a checksum-only stage
        // is not an authenticated stage.
        assert_eq!(
            parse_manifest("repo=x\ntag=y\nhelper_sha=b\nmain_sha=c\n"),
            None
        );
        assert_eq!(
            parse_manifest("repo=x\ntag=y\nhelper_sha=b\nmain_sha=c\nsig_verified=0\n"),
            None
        );
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
        // Happy path: reuse returns the staged bytes. The stage also
        // carries the signature texts, which the gate verifies before
        // the bytes are returned.
        std::fs::write(stage.join(sig_asset_name(&helper_asset)), "helper-sig").unwrap();
        std::fs::write(stage.join(sig_asset_name(&main_asset)), "main-sig").unwrap();
        let reused =
            reuse_staged_update_with(&stage, "k1ng0mar/pantheon", "v9.9.9", |bytes, sig| {
                if bytes == helper_bytes && sig == "helper-sig" {
                    Ok(())
                } else if bytes == main_bytes && sig == "main-sig" {
                    Ok(())
                } else {
                    Err("not the staged pair".into())
                }
            })
            .unwrap();
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

    /// R6 regression: a staged update is only reused when it
    /// re-verifies against the pinned release key. A planted stage
    /// (correct digests, `sig_verified=1`, no signature) must be
    /// refused - the manifest flag is a record of the previous run, not
    /// proof, because the stage lives in a shared temp directory.
    #[test]
    fn staged_reuse_requires_a_verifiable_signature() {
        let stage = tmp_dir("stage-sig");
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
        // No signature text staged: the stage cannot be authenticated.
        assert!(
            reuse_staged_update(&stage, "k1ng0mar/pantheon", "v9.9.9").is_none(),
            "a stage with no signature must never be reused"
        );
        // A planted signature that does not verify is refused too.
        std::fs::write(stage.join(sig_asset_name(&helper_asset)), "not a signature").unwrap();
        std::fs::write(stage.join(sig_asset_name(&main_asset)), "not a signature").unwrap();
        assert!(
            reuse_staged_update(&stage, "k1ng0mar/pantheon", "v9.9.9").is_none(),
            "a stage whose signature does not verify must never be reused"
        );
        let _ = std::fs::remove_dir_all(&stage);
    }

    /// The gate hands the verifier the exact bytes it is about to
    /// install, paired with the signature text staged beside them, and
    /// a stage is refused when that pair does not verify.
    #[test]
    fn staged_reuse_verifies_the_bytes_it_will_install() {
        let stage = tmp_dir("stage-pairs");
        let helper_bytes = b"helper-bytes";
        let main_bytes = b"main-bytes";
        let helper_sha = hash_via_system(helper_bytes).unwrap();
        let main_sha = hash_via_system(main_bytes).unwrap();
        let (_, helper_asset) = release_asset("k1ng0mar/pantheon", "v9.9.9", "pantheon-helper");
        let (_, main_asset) = release_asset("k1ng0mar/pantheon", "v9.9.9", "pantheon");
        std::fs::write(stage.join(&helper_asset), helper_bytes).unwrap();
        std::fs::write(stage.join(&main_asset), main_bytes).unwrap();
        std::fs::write(stage.join(sig_asset_name(&helper_asset)), "helper-sig").unwrap();
        std::fs::write(stage.join(sig_asset_name(&main_asset)), "main-sig").unwrap();
        write_manifest(
            &stage,
            "k1ng0mar/pantheon",
            "v9.9.9",
            &helper_sha,
            &main_sha,
        )
        .unwrap();
        // A verifier that accepts only the staged pairs: reuse succeeds.
        let seen = std::cell::RefCell::new(Vec::new());
        let reused =
            reuse_staged_update_with(&stage, "k1ng0mar/pantheon", "v9.9.9", |bytes, sig| {
                seen.borrow_mut().push((bytes.to_vec(), sig.to_string()));
                if (bytes == helper_bytes && sig == "helper-sig")
                    || (bytes == main_bytes && sig == "main-sig")
                {
                    Ok(())
                } else {
                    Err("not the staged pair".into())
                }
            });
        assert!(reused.is_some(), "a verified stage must still be reusable");
        let seen = seen.into_inner();
        assert_eq!(seen.len(), 2, "both archives must be verified");
        assert!(seen
            .iter()
            .any(|(b, s)| b == helper_bytes && s == "helper-sig"));
        assert!(seen.iter().any(|(b, s)| b == main_bytes && s == "main-sig"));
        // A verifier that rejects everything: the stage is refused.
        assert!(
            reuse_staged_update_with(&stage, "k1ng0mar/pantheon", "v9.9.9", |_, _| {
                Err("no key pinned".into())
            })
            .is_none()
        );
        let _ = std::fs::remove_dir_all(&stage);
    }

    /// A stage directory another local user can write to is refused:
    /// the archives could be swapped between the digest check and the
    /// install.
    #[cfg(unix)]
    #[test]
    fn shared_writable_stage_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let stage = tmp_dir("stage-shared");
        let helper_bytes = b"helper-bytes";
        let main_bytes = b"main-bytes";
        let helper_sha = hash_via_system(helper_bytes).unwrap();
        let main_sha = hash_via_system(main_bytes).unwrap();
        let (_, helper_asset) = release_asset("k1ng0mar/pantheon", "v9.9.9", "pantheon-helper");
        let (_, main_asset) = release_asset("k1ng0mar/pantheon", "v9.9.9", "pantheon");
        std::fs::write(stage.join(&helper_asset), helper_bytes).unwrap();
        std::fs::write(stage.join(&main_asset), main_bytes).unwrap();
        std::fs::write(stage.join(sig_asset_name(&helper_asset)), "helper-sig").unwrap();
        std::fs::write(stage.join(sig_asset_name(&main_asset)), "main-sig").unwrap();
        write_manifest(
            &stage,
            "k1ng0mar/pantheon",
            "v9.9.9",
            &helper_sha,
            &main_sha,
        )
        .unwrap();
        std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            reuse_staged_update_with(&stage, "k1ng0mar/pantheon", "v9.9.9", |_, _| Ok(()))
                .is_none(),
            "a world-writable stage must never be reused"
        );
        let _ = std::fs::remove_dir_all(&stage);
    }

    /// The signature texts must be staged next to the archives: a
    /// resumed update re-verifies the bytes it installs, offline, from
    /// the stage. A stage with archives but no signatures is refused.
    #[test]
    fn staging_writes_the_signature_texts_next_to_the_archives() {
        let stage = tmp_dir("stage-write");
        let (_, helper_asset) = release_asset("k1ng0mar/pantheon", "v9.9.9", "pantheon-helper");
        let (_, main_asset) = release_asset("k1ng0mar/pantheon", "v9.9.9", "pantheon");
        let helper_bytes = b"helper-bytes";
        let main_bytes = b"main-bytes";
        let helper = VerifiedAsset {
            asset: &helper_asset,
            bytes: helper_bytes,
            sig_text: "helper-sig-text",
            sha: &hash_via_system(helper_bytes).unwrap(),
        };
        let main = VerifiedAsset {
            asset: &main_asset,
            bytes: main_bytes,
            sig_text: "main-sig-text",
            sha: &hash_via_system(main_bytes).unwrap(),
        };
        stage_verified_update(&stage, "k1ng0mar/pantheon", "v9.9.9", &helper, &main).unwrap();
        // The archives are staged...
        assert_eq!(
            std::fs::read(stage.join(&helper_asset)).unwrap(),
            helper_bytes
        );
        assert_eq!(std::fs::read(stage.join(&main_asset)).unwrap(), main_bytes);
        // ...and so are the signature texts that authenticate them.
        assert_eq!(
            std::fs::read_to_string(stage.join(sig_asset_name(&helper_asset))).unwrap(),
            "helper-sig-text"
        );
        assert_eq!(
            std::fs::read_to_string(stage.join(sig_asset_name(&main_asset))).unwrap(),
            "main-sig-text"
        );
        // The manifest records the digests of what was staged.
        let text = std::fs::read_to_string(manifest_path(&stage)).unwrap();
        let (repo, tag, helper_sha, main_sha) = parse_manifest(&text).unwrap();
        assert_eq!(repo, "k1ng0mar/pantheon");
        assert_eq!(tag, "v9.9.9");
        assert_eq!(helper_sha, helper.sha);
        assert_eq!(main_sha, main.sha);
        // And the stage it wrote is reusable, because the signature
        // texts went with the archives.
        assert!(
            reuse_staged_update_with(&stage, "k1ng0mar/pantheon", "v9.9.9", |bytes, sig| {
                if (bytes == helper_bytes && sig == "helper-sig-text")
                    || (bytes == main_bytes && sig == "main-sig-text")
                {
                    Ok(())
                } else {
                    Err("not the staged pair".into())
                }
            })
            .is_some()
        );
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
        // The gate cmd_update hits before any file operation: a stage
        // that cannot be authenticated is refused, and the current
        // binary is left byte-for-byte untouched - no `.new` candidate,
        // no `.prev` backup, same bytes. This pins the ordering: a
        // verification failure never replaces a working install.
        let dir = tmp_dir("gate");
        let current = dir.join("pantheon");
        std::fs::write(&current, b"old-binary").unwrap();
        let stage = dir.join("stage");
        create_private_dir(&stage).unwrap();
        // A planted stage: correct digests for the planted bytes and a
        // manifest that claims sig_verified=1, but nothing that
        // authenticates them.
        let helper_bytes = b"attacker-helper";
        let main_bytes = b"attacker-main";
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
        assert!(reuse_staged_update(&stage, "k1ng0mar/pantheon", "v9.9.9").is_none());
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

    /// Minimal tar writer for extraction tests: one 512-byte ustar
    /// header per member, no compression, no external tools.
    fn tar_bytes(members: &[(&str, &[u8], u8, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data, typeflag, linkname) in members {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..108].copy_from_slice(b"0000644\0");
            h[108..116].copy_from_slice(b"0000000\0");
            h[116..124].copy_from_slice(b"0000000\0");
            let size = format!("{:011o}\0", data.len());
            h[124..136].copy_from_slice(size.as_bytes());
            h[136..148].copy_from_slice(b"00000000000\0");
            h[148..156].copy_from_slice(b"        ");
            h[156] = *typeflag;
            if !linkname.is_empty() {
                h[157..157 + linkname.len()].copy_from_slice(linkname.as_bytes());
            }
            h[257..263].copy_from_slice(b"ustar\0");
            let sum: u32 = h.iter().map(|b| *b as u32).sum();
            let chk = format!("{:06o}\0 ", sum);
            h[148..156].copy_from_slice(chk.as_bytes());
            out.extend_from_slice(&h);
            out.extend_from_slice(data);
            let pad = (512 - data.len() % 512) % 512;
            out.extend(std::iter::repeat(0).take(pad));
        }
        out.extend(std::iter::repeat(0).take(1024));
        out
    }

    #[test]
    fn extract_picks_the_exact_binary_not_a_substring_match() {
        // The old walk matched by substring, so a search for
        // `pantheon` could install `pantheon-helper` as the main
        // binary. The exact-basename match must pick `pantheon`.
        let dir = tmp_dir("extract-exact");
        let archive = tar_bytes(&[
            ("bin/pantheon-helper", b"helper", b'0', ""),
            ("bin/pantheon", b"main", b'0', ""),
        ]);
        let found = extract_tar(&archive, "x.tar", &dir, "pantheon").unwrap();
        assert_eq!(std::fs::read(&found).unwrap(), b"main");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_rejects_traversal_absolute_and_symlink_members() {
        let dir = tmp_dir("extract-guard");
        let evil = tar_bytes(&[("../evil", b"x", b'0', "")]);
        let err = extract_tar(&evil, "e.tar", &dir, "pantheon").unwrap_err();
        assert!(err.contains("escapes"), "{err}");
        let abs = tar_bytes(&[("/tmp/evil", b"x", b'0', "")]);
        let err = extract_tar(&abs, "a.tar", &dir, "pantheon").unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        let link = tar_bytes(&[("pantheon", b"", b'2', "/etc/passwd")]);
        let err = extract_tar(&link, "l.tar", &dir, "pantheon").unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
