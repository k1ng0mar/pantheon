//! `pantheon update`: replace this binary with the latest GitHub release.
//!
//! The installer downloads a prebuilt binary; this verb is the same path
//! going forward. No toolchain, no source build: resolve the release,
//! download the matching asset for this OS/arch, verify the checksum when
//! published, and swap the running binary.
//!
//! Failures name the exact stage (resolve, download, verify, replace) and
//! a timeout names the probe that timed out - never a generic "update
//! failed".

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_REPO: &str = "k1ng0mar/pantheon";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

fn repo() -> String {
    std::env::var("PANTHEON_REPO").unwrap_or_else(|_| DEFAULT_REPO.into())
}

fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Asset OS id for this build.
fn os_id() -> Option<&'static str> {
    match std::env::consts::OS {
        "linux" => Some("linux"),
        "macos" => Some("darwin"),
        "windows" => Some("windows"),
        _ => None,
    }
}

/// Asset arch id for this build.
fn arch_id() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("aarch64"),
        _ => None,
    }
}

/// Asset file name for a release tag, e.g.
/// `pantheon-v0.1.0-linux-x86_64.tar.gz`.
fn asset_name(tag: &str, os: &str, arch: &str) -> String {
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    format!("pantheon-{tag}-{os}-{arch}.{ext}")
}

fn download_url(repo: &str, tag: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
}

/// True when `latest` is newer than `current`. Tags carry a leading `v`;
/// comparison is numeric per dot-separated component, falling back to
/// "different means newer" for anything non-numeric.
fn is_newer(current: &str, latest: &str) -> bool {
    let norm = |v: &str| v.trim().trim_start_matches('v').to_string();
    let (c, l) = (norm(current), norm(latest));
    if c == l {
        return false;
    }
    let parse = |v: &str| {
        v.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    parse(&l) > parse(&c)
}

/// Agent for update traffic: bounded, platform roots, default redirect
/// following (release downloads redirect to a CDN).
fn update_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

/// Resolve the latest release tag via the GitHub API.
fn resolve_latest_tag(repo: &str) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = update_agent()
        .get(&url)
        .header("User-Agent", "pantheon-update")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => {
                format!("resolve failed: GitHub API returned HTTP {code} for {repo}")
            }
            e => format!("resolve failed: could not reach api.github.com ({e})"),
        })?;
    let text = resp
        .into_body()
        .read_to_string()
        .map_err(|e| format!("resolve failed: could not read GitHub response ({e})"))?;
    // Minimal parse: find "tag_name": "vX.Y.Z" without serde_json::Value
    // plumbing (serde_json is available, but a full struct is overkill).
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("resolve failed: bad JSON ({e})"))?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "resolve failed: GitHub response had no tag_name".to_string())
}

fn download_bytes(url: &str) -> Result<Vec<u8>, String> {
    let resp = update_agent()
        .get(url)
        .header("User-Agent", "pantheon-update")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => {
                format!("download failed: HTTP {code} for {url}")
            }
            e => format!("download failed: could not fetch {url} ({e})"),
        })?;
    let mut buf = Vec::new();
    resp.into_body()
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("download failed: could not read {url} ({e})"))?;
    Ok(buf)
}

/// Extract the `pantheon` binary from `archive` into `dest_dir` using the
/// system `tar` (the installer guarantees it; Windows 10+ ships one that
/// handles zip). Returns the extracted binary path.
fn extract_with_tar(
    archive: &std::path::Path,
    dest_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // List first and reject traversal before extracting anything: a
    // hostile archive must not carry absolute paths or `..` members
    // out of the temp dir. Then extract ONLY the single member whose
    // basename is exactly the wanted binary (fail on zero or on
    // ambiguity) and require it to be a regular file, never a symlink.
    // The old code extracted the whole archive and walked the tree,
    // which let a symlink member become the installed binary.
    let list = std::process::Command::new("tar")
        .arg("-tf")
        .arg(archive)
        .output()
        .map_err(|e| {
            format!("install failed: could not run `tar` ({e}); install tar and re-run")
        })?;
    if !list.status.success() {
        return Err("install failed: `tar` could not list the release archive".into());
    }
    let names: &[&str] = if cfg!(windows) {
        &["pantheon.exe"]
    } else {
        &["pantheon"]
    };
    let mut candidates: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&list.stdout).lines() {
        let entry = line.trim();
        if entry.is_empty() {
            continue;
        }
        if entry.starts_with('/') || entry.starts_with('\\') {
            return Err(format!("install failed: absolute archive member {entry:?}"));
        }
        if entry
            .split(|c| c == '/' || c == '\\')
            .any(|seg| seg == "..")
        {
            return Err(format!(
                "install failed: archive member escapes its directory: {entry:?}"
            ));
        }
        let base = entry
            .rsplit(|c| c == '/' || c == '\\')
            .next()
            .unwrap_or(entry);
        if names.contains(&base) {
            candidates.push(entry.to_string());
        }
    }
    if candidates.is_empty() {
        return Err("install failed: archive did not contain a pantheon binary".into());
    }
    if candidates.len() > 1 {
        return Err(format!(
            "install failed: ambiguous archive ({} members named like the binary)",
            candidates.len()
        ));
    }
    let member = candidates.into_iter().next().unwrap();
    // NOTE: `-C dest` must precede the member name: tar applies -C
    // to the arguments that follow it, so `-C` after the member
    // would extract into the process cwd instead of dest.
    let st = std::process::Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest_dir)
        .arg(&member)
        .status()
        .map_err(|e| {
            format!("install failed: could not run `tar` ({e}); install tar and re-run")
        })?;
    if !st.success() {
        return Err("install failed: `tar` could not extract the release archive".into());
    }
    let extracted = dest_dir.join(member.strip_prefix("./").unwrap_or(member.as_str()));
    match std::fs::symlink_metadata(&extracted) {
        Ok(md) if md.file_type().is_file() => Ok(extracted),
        Ok(_) => Err("install failed: extracted binary is not a regular file".into()),
        Err(e) => Err(format!(
            "install failed: cannot read {} ({e})",
            extracted.display()
        )),
    }
}

/// Pure checksum comparison, extracted for testing: find the digest
/// for `asset` in the published `.sha256` text (plain `<hex>` lines,
/// `<hex>  <file>` sha256sum lines, and `SHA256 (file) = <hex>` BSD
/// lines all work) and compare it against `actual_hex`.
///
/// The digest is bound to the asset when the file names it: if any
/// line names the asset, that line's digest wins. Otherwise the file
/// must carry exactly one digest; a multi-digest file that never
/// names the asset is ambiguous and counts as "no usable digest"
/// (fail closed) rather than the first token, which might belong to
/// another file.
///
/// NOTE: a checksum only proves the bytes match the publisher's
/// published digest when the digest itself is authentic. The
/// `.sha256` file is fetched from the same origin as the binary, so
/// a compromised release can serve a matching pair. Real
/// authentication needs signatures (see NEEDS DECISION in the
/// security fix report); until then this is a corruption check, not
/// a trust anchor.
///
/// Returns Ok(true) on match, Ok(false) when the text carries no usable
/// digest, Err on mismatch.
fn checksum_line_matches(text: &str, actual_hex: &str, asset: &str) -> Result<bool, String> {
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
            return check_digest(&found[0], actual_hex);
        }
        all.extend(found);
    }
    if all.len() == 1 {
        return check_digest(&all[0], actual_hex);
    }
    Ok(false)
}

fn check_digest(expected: &str, actual_hex: &str) -> Result<bool, String> {
    if actual_hex.to_lowercase() != expected {
        return Err("verify failed: checksum mismatch (download may be corrupt)".into());
    }
    Ok(true)
}

/// Gate a verification outcome: only a digest match installs.
/// Ok(false) — checksum file exists but carries no usable digest — is
/// an error here, not a silent skip: the caller must not treat "no
/// digest found" as "verified". Pure, for testing.
fn require_verified(verified: Result<bool, String>) -> Result<(), String> {
    match verified {
        Ok(true) => Ok(()),
        Ok(false) => Err("verify failed: checksum file carries no usable digest".into()),
        Err(e) => Err(e),
    }
}

/// Verify `asset_bytes` against the published `.sha256` file.
///
/// Fail-closed: a missing checksum file (404), any network error, or an
/// unreadable checksum file is an error, not a silent skip. Returns
/// Ok(true) only when the digest matches; Ok(false) only when the
/// checksum file exists but carries no usable digest (callers must
/// pass this through [`require_verified`], never install on it).
fn verify_checksum(asset_url: &str, asset_bytes: &[u8]) -> Result<bool, String> {
    let sha_url = format!("{asset_url}.sha256");
    let resp = update_agent()
        .get(&sha_url)
        .header("User-Agent", "pantheon-update")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => {
                format!("verify failed: checksum file returned HTTP {code} ({sha_url})")
            }
            _ => format!("verify failed: could not fetch the checksum file ({e})"),
        })?;
    let text = resp
        .into_body()
        .read_to_string()
        .map_err(|_| "verify failed: could not read the checksum file".to_string())?;
    // Hash via the system: sha256sum, shasum, or certutil on Windows.
    let actual = hash_via_system(asset_bytes)?;
    let asset = asset_url.rsplit('/').next().unwrap_or("");
    checksum_line_matches(&text, &actual, asset)
}

fn hash_via_system(bytes: &[u8]) -> Result<String, String> {
    use std::io::Write;
    for (prog, args) in [
        ("sha256sum", vec![] as Vec<&str>),
        ("shasum", vec!["-a", "256"]),
    ] {
        if let Ok(mut child) = std::process::Command::new(prog)
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
    if cfg!(windows) {
        // certutil needs a file; write to temp.
        let tmp = std::env::temp_dir().join(format!("pantheon-hash-{}.bin", std::process::id()));
        std::fs::write(&tmp, bytes).map_err(|e| format!("verify failed: {e}"))?;
        let out = std::process::Command::new("certutil")
            .args(["-hashfile", &tmp.to_string_lossy(), "SHA256"])
            .output()
            .map_err(|e| format!("verify failed: {e}"))?;
        let _ = std::fs::remove_file(&tmp);
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            if t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(t);
            }
        }
    }
    Err("verify failed: no sha256 tool found (install sha256sum)".into())
}

/// Splash hint probe: return the newer release tag when one exists,
/// `None` when up to date or unreachable. Network failures are silent
/// the splash simply shows no hint.
pub fn check_for_update() -> Option<String> {
    let latest = resolve_latest_tag(&repo()).ok()?;
    if is_newer(&current_version(), &latest) {
        Some(latest)
    } else {
        None
    }
}

pub fn usage() -> &'static str {
    "usage: pantheon update [--check] [--version TAG] [--repo OWNER/REPO] [--rollback]\n  \n  Fetches the latest GitHub release and replaces this binary.\n  --check reports without changing anything.\n  --rollback restores the previous binary (pantheon.prev, kept next to\n  the binary by every update) without touching the network.\n  Verification is fail-closed: a missing, unreadable, digest-less, or mismatched\n  checksum aborts the update. There is no --allow-unverified flag: an unverified binary is never installed."
}

/// `pantheon update --rollback`: restore the previous binary from the
/// `pantheon.prev` backup the last update left next to this binary. No
/// network, no version resolution - the backup is the rollback. Also
/// restarts any systemd units that were stopped during the update.
fn cmd_rollback() {
    let current_exe = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("rollback: cannot locate the running binary ({e})");
        std::process::exit(1);
    });
    let backup = current_exe.with_extension("prev");
    if !backup.is_file() {
        eprintln!(
            "rollback: no backup at {} (updates keep one; none has run yet)",
            backup.display()
        );
        std::process::exit(1);
    }
    if std::fs::copy(&backup, &current_exe).is_err() {
        eprintln!(
            "rollback: cannot restore {} over {}",
            backup.display(),
            current_exe.display()
        );
        std::process::exit(1);
    }
    println!("✓ Rolled back to {}", backup.display());
    println!("  Binary: {}", current_exe.display());
    // Roll the helper back too: the update backs it up to
    // `pantheon-helper.prev` before swapping. A rollback that restores
    // only the main binary leaves a new helper driving an old main.
    // No helper backup (updates from before this fix) is a note, not
    // a failure; a failed helper restore is a warning, since the main
    // binary - the thing that runs - is restored and the helper
    // self-heals on the next update.
    let helper_exe = current_exe.with_file_name("pantheon-helper");
    let helper_backup = helper_exe.with_extension("prev");
    if helper_backup.is_file() {
        match std::fs::copy(&helper_backup, &helper_exe) {
            Ok(_) => println!("✓ Rolled helper back to {}", helper_backup.display()),
            Err(e) => eprintln!("rollback: ⚠ could not restore helper ({e})"),
        }
    }
    // Restart any systemd units that were stopped during the update.
    // The state file is removed only when every restart succeeded:
    // deleting it after a failed restart would lose the only record
    // of which units are still down.
    let state_path = current_exe.with_extension("update-state");
    if state_path.is_file() {
        let mut restarts_ok = true;
        if let Ok(text) = std::fs::read_to_string(&state_path) {
            for unit in text.lines() {
                if !unit.trim().is_empty() {
                    if restart_systemd_unit_ok(unit.trim()) {
                        println!("  Restarted {}", unit.trim());
                    } else {
                        eprintln!(
                            "rollback: ⚠ could not restart {}; restart it manually",
                            unit.trim()
                        );
                        restarts_ok = false;
                    }
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
}

pub fn cmd_update(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return;
    }
    // Unknown flags used to fall through to a network fetch: `update
    // --bogus` cheerfully replaced the binary while ignoring whatever
    // the operator thought they had asked for.
    // --version/--repo take a value (`--version v1.2.3` or
    // `--version=v1.2.3`, as `flag_val` reads them), so the value
    // token is consumed here, not mistaken for a stray positional.
    // --check/--rollback are boolean switches.
    let rest: &[String] = args.get(2..).unwrap_or(&[]);
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if a.starts_with("--") {
            let name = a.split('=').next().unwrap_or(a.as_str());
            if !matches!(name, "--check" | "--version" | "--repo" | "--rollback") {
                eprintln!("update: unknown flag '{a}'");
                eprintln!("{}", usage());
                std::process::exit(2);
            }
            if matches!(name, "--version" | "--repo") && !a.contains('=') {
                match rest.get(i + 1) {
                    Some(v) if !v.starts_with("--") => i += 1,
                    _ => {
                        eprintln!("update: {name} needs a value");
                        eprintln!("{}", usage());
                        std::process::exit(2);
                    }
                }
            }
        } else if a.starts_with('-') {
            eprintln!("update: unknown flag '{a}'");
            eprintln!("{}", usage());
            std::process::exit(2);
        } else {
            eprintln!("update: unexpected argument '{a}'");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
        i += 1;
    }
    let flag_val = |name: &str| {
        let mut it = args.iter().peekable();
        while let Some(a) = it.next() {
            if a == name {
                return it.next().cloned();
            }
            if let Some(v) = a.strip_prefix(&format!("{name}=")) {
                return Some(v.to_string());
            }
        }
        None
    };
    let check_only = args.iter().any(|a| a == "--check");
    if args.iter().any(|a| a == "--rollback") {
        return cmd_rollback();
    }
    let repo = flag_val("--repo").unwrap_or_else(repo);
    let os = match os_id() {
        Some(o) => o,
        None => {
            eprintln!(
                "update: unsupported OS {} - download a release by hand from https://github.com/{repo}/releases",
                std::env::consts::OS
            );
            std::process::exit(1);
        }
    };
    let arch = match arch_id() {
        Some(a) => a,
        None => {
            eprintln!(
                "update: unsupported architecture {} - build from source instead",
                std::env::consts::ARCH
            );
            std::process::exit(1);
        }
    };

    let tag = match flag_val("--version") {
        Some(v) => v,
        None => resolve_latest_tag(&repo).unwrap_or_else(|e| {
            eprintln!("update: {e}");
            std::process::exit(1);
        }),
    };

    let current = current_version();
    if !is_newer(&current, &tag) && flag_val("--version").is_none() {
        println!("pantheon {current} is already the latest ({tag})");
        return;
    }

    if check_only {
        if is_newer(&current, &tag) {
            println!("update available: pantheon {current} → {tag}");
            println!("run `pantheon update` to install it");
        } else {
            println!("pantheon {current} is already the latest ({tag})");
        }
        return;
    }

    // Handoff pattern: spawn pantheon-helper detached, then exit.
    // The helper does the actual download, verify, swap, and restart.
    // This lets even old binaries self-update, because the helper survives
    // the binary swap (it has its own copy in memory).
    let current_exe = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("update: install failed: cannot locate the running binary ({e})");
        std::process::exit(1);
    });
    let helper_exe = current_exe.with_file_name("pantheon-helper");

    // Download the helper binary if it's missing or outdated.
    if !helper_exe.is_file() {
        println!("· Downloading pantheon-helper");
        let helper_asset = asset_name(&tag, os, arch).replace("pantheon-", "pantheon-helper-");
        let helper_url = download_url(&repo, &tag, &helper_asset);
        let helper_bytes = match download_bytes(&helper_url) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("update: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = require_verified(verify_checksum(&helper_url, &helper_bytes)) {
            eprintln!("update: {e}");
            std::process::exit(1);
        }
        let tmp = std::env::temp_dir().join(format!("pantheon-helper-{}", std::process::id()));
        let helper_archive = tmp.join(&helper_asset);
        let _ = std::fs::create_dir_all(&tmp);
        let _ = std::fs::write(&helper_archive, &helper_bytes);
        let extracted = match extract_with_tar(&helper_archive, &tmp) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("update: {e}");
                std::process::exit(1);
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(md) = std::fs::metadata(&extracted) {
                let mut perm = md.permissions();
                perm.set_mode(0o755);
                let _ = std::fs::set_permissions(&extracted, perm);
            }
        }
        if let Err(e) = std::fs::rename(&extracted, &helper_exe) {
            eprintln!("update: cannot install helper ({e})");
            std::process::exit(1);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Spawn the helper detached and exit.
    println!("· Spawning update helper");
    let mut cmd = std::process::Command::new(&helper_exe);
    cmd.arg("update")
        .arg("--current")
        .arg(&current_exe)
        .arg("--tag")
        .arg(&tag)
        .arg("--repo")
        .arg(&repo);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let _ = child.wait();
            // Exit so the helper can swap the binary.
            println!("✓ Update in progress (helper running)");
        }
        Err(e) => {
            eprintln!("update: cannot spawn helper ({e})");
            std::process::exit(1);
        }
    }
}

/// Restart a systemd unit (Unix) or Windows service, reporting
/// success so rollback can keep the state file when a restart fails
/// instead of deleting the only record of which units are still down.
fn restart_systemd_unit_ok(unit: &str) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("systemctl")
            .args(["--user", "start", unit])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("sc")
            .args(["start", unit])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::require_verified;

    #[test]
    fn state_file_round_trips_units_for_rollback() {
        let exe = std::env::current_exe().unwrap();
        let state_path = exe.with_extension("update-state");
        let units = ["pantheon-gateway.service".to_string()];
        std::fs::write(&state_path, units.join("\n")).unwrap();
        let text = std::fs::read_to_string(&state_path).unwrap();
        let parsed: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(parsed, vec!["pantheon-gateway.service"]);
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn verification_gate_rejects_digest_less_and_failed_checks() {
        // Only a digest match installs. A checksum file with no usable
        // digest used to pass through as Ok(false) in the helper
        // download path; require_verified closes that hole.
        assert!(require_verified(Ok(true)).is_ok());
        assert!(require_verified(Ok(false)).is_err());
        assert_eq!(
            require_verified(Err("verify failed: checksum mismatch".to_string())).unwrap_err(),
            "verify failed: checksum mismatch"
        );
    }

    #[test]
    fn find_running_excludes_self() {
        let pids = find_running_pantheon_processes();
        assert!(!pids.contains(&std::process::id()));
    }

    #[cfg(test)]
    fn find_running_pantheon_processes() -> Vec<u32> {
        let current_pid = std::process::id();
        let current_exe = std::env::current_exe().unwrap_or_default();
        let exe_name = current_exe
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("pantheon")
            .to_string();
        let mut pids = Vec::new();

        #[cfg(unix)]
        {
            if let Ok(out) = std::process::Command::new("pgrep")
                .arg("-x")
                .arg(&exe_name)
                .output()
            {
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
            if let Ok(out) = std::process::Command::new("tasklist")
                .args([
                    "/FI",
                    &format!("IMAGENAME eq {exe_name}"),
                    "/NH",
                    "/FO",
                    "CSV",
                ])
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

        pids
    }

    #[test]
    fn permission_mode_resolve_behavior_verified() {
        use pantheon_api::capability::Decision;
        use pantheon_api::model::GateVerdict;
        use pantheon_api::permission_mode::PermissionMode;
        for mode in [
            PermissionMode::Ask,
            PermissionMode::Smart,
            PermissionMode::AllowAll,
        ] {
            assert_eq!(
                mode.resolve(Decision::Deny, Some(GateVerdict::Allow)),
                Decision::Deny
            );
        }
        assert_eq!(
            PermissionMode::Ask.resolve(Decision::Approval, None),
            Decision::Approval
        );
        assert_eq!(
            PermissionMode::AllowAll.resolve(Decision::Approval, None),
            Decision::Allow
        );
    }
}
