//! `pantheon update`: replace this binary with the latest GitHub release.
//!
//! The installer downloads a prebuilt binary; this verb is the same path
//! going forward. No toolchain, no source build: resolve the release,
//! download the matching asset for this OS/arch, verify the checksum when
//! published, and swap the running binary.
//!
//! Failures name the exact stage (resolve, download, verify, replace) and
//! a timeout names the probe that timed out — never a generic "update
//! failed".

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

/// Resolve the latest release tag via the GitHub API.
fn resolve_latest_tag(repo: &str) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = ureq::get(&url)
        .set("User-Agent", "pantheon-update")
        .set("Accept", "application/vnd.github+json")
        .timeout(REQUEST_TIMEOUT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Transport(t) => {
                format!("resolve failed: could not reach api.github.com ({t})")
            }
            ureq::Error::Status(code, _) => {
                format!("resolve failed: GitHub API returned HTTP {code} for {repo}")
            }
        })?;
    let text = resp
        .into_string()
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
    let resp = ureq::get(url)
        .set("User-Agent", "pantheon-update")
        .timeout(REQUEST_TIMEOUT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Transport(t) => {
                format!("download failed: could not fetch {url} ({t})")
            }
            ureq::Error::Status(code, _) => {
                format!("download failed: HTTP {code} for {url}")
            }
        })?;
    let mut buf = Vec::new();
    resp.into_reader()
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
    let st = std::process::Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .map_err(|e| {
            format!("install failed: could not run `tar` ({e}); install tar and re-run")
        })?;
    if !st.success() {
        return Err("install failed: `tar` could not extract the release archive".into());
    }
    let names: &[&str] = if cfg!(windows) {
        &["pantheon.exe"]
    } else {
        &["pantheon"]
    };
    let mut found = None;
    let mut stack = vec![dest_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("install failed: cannot read {} ({e})", dir.display()))?;
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if names.contains(&name) {
                    found = Some(p);
                    break;
                }
            }
        }
        if found.is_some() {
            break;
        }
    }
    found.ok_or_else(|| "install failed: archive did not contain a pantheon binary".into())
}

/// Pure checksum comparison, extracted for testing: find the first
/// 64-hex token in the published `.sha256` text (plain `<hex>` lines,
/// `<hex>  <file>` sha256sum lines, and `SHA256 (file) = <hex>` BSD
/// lines all work) and compare it against `actual_hex`.
///
/// Returns Ok(true) on match, Ok(false) when the text carries no usable
/// digest, Err on mismatch.
fn checksum_line_matches(text: &str, actual_hex: &str) -> Result<bool, String> {
    let expected = text
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| c == '(' || c == ')' || c == '=' || c == '*')
                .to_lowercase()
        })
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_default();
    if expected.is_empty() {
        return Ok(false);
    }
    if actual_hex.to_lowercase() != expected {
        return Err("verify failed: checksum mismatch (download may be corrupt)".into());
    }
    Ok(true)
}

/// Verify `asset_bytes` against the published `.sha256` file.
///
/// Fail-closed: a missing checksum file (404), any network error, or an
/// unreadable checksum file is an error, not a silent skip. Returns
/// Ok(true) only when the digest matches; Ok(false) only when the
/// checksum file exists but carries no usable digest.
fn verify_checksum(asset_url: &str, asset_bytes: &[u8]) -> Result<bool, String> {
    let sha_url = format!("{asset_url}.sha256");
    let resp = ureq::get(&sha_url)
        .set("User-Agent", "pantheon-update")
        .timeout(REQUEST_TIMEOUT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => {
                format!("verify failed: checksum file returned HTTP {code} ({sha_url})")
            }
            _ => format!("verify failed: could not fetch the checksum file ({e})"),
        })?;
    let text = resp
        .into_string()
        .map_err(|_| "verify failed: could not read the checksum file".to_string())?;
    // Hash via the system: sha256sum, shasum, or certutil on Windows.
    let actual = hash_via_system(asset_bytes)?;
    checksum_line_matches(&text, &actual)
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

pub fn usage() -> &'static str {
    "usage: pantheon update [--check] [--version TAG] [--repo OWNER/REPO] [--allow-unverified]\n  \n  Fetches the latest GitHub release and replaces this binary.\n  --check reports without changing anything.\n  Verification is fail-closed: a missing, unreadable, or mismatched\n  checksum aborts the update. Pass --allow-unverified to install anyway."
}

pub fn cmd_update(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return;
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
    let allow_unverified = args.iter().any(|a| a == "--allow-unverified");
    let repo = flag_val("--repo").unwrap_or_else(repo);
    let os = match os_id() {
        Some(o) => o,
        None => {
            eprintln!(
                "update: unsupported OS {} — download a release by hand from https://github.com/{repo}/releases",
                std::env::consts::OS
            );
            std::process::exit(1);
        }
    };
    let arch = match arch_id() {
        Some(a) => a,
        None => {
            eprintln!(
                "update: unsupported architecture {} — build from source instead",
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

    let asset = asset_name(&tag, os, arch);
    let url = download_url(&repo, &tag, &asset);
    if check_only {
        if is_newer(&current, &tag) {
            println!("update available: pantheon {current} → {tag}");
            println!("run `pantheon update` to install it");
        } else {
            println!("pantheon {current} is already the latest ({tag})");
        }
        return;
    }

    println!("· Downloading Pantheon {tag}");
    let bytes = download_bytes(&url).unwrap_or_else(|e| {
        eprintln!("update: {e}");
        std::process::exit(1);
    });

    println!("· Verifying release");
    // Fail-closed: any verification problem aborts the update unless the
    // user explicitly passed --allow-unverified.
    match verify_checksum(&url, &bytes) {
        Ok(true) => println!("✓ Checksum verified"),
        Ok(false) => {
            if allow_unverified {
                eprintln!(
                    "⚠ no usable checksum published for {asset}; installing unverified (--allow-unverified)"
                );
            } else {
                eprintln!(
                    "update: verify failed: no usable checksum published for {asset}; refusing to install (re-run with --allow-unverified to override)"
                );
                std::process::exit(1);
            }
        }
        Err(e) => {
            if allow_unverified {
                eprintln!("⚠ {e}; installing unverified (--allow-unverified)");
            } else {
                eprintln!("update: {e}");
                std::process::exit(1);
            }
        }
    }

    println!("· Installing binary");
    let tmp = std::env::temp_dir().join(format!("pantheon-update-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        eprintln!(
            "update: install failed: cannot create {} ({e})",
            tmp.display()
        );
        std::process::exit(1);
    }
    let archive_path = tmp.join(&asset);
    if let Err(e) = std::fs::write(&archive_path, &bytes) {
        eprintln!(
            "update: install failed: cannot write {} ({e})",
            archive_path.display()
        );
        std::process::exit(1);
    }
    let new_bin = extract_with_tar(&archive_path, &tmp).unwrap_or_else(|e| {
        eprintln!("update: {e}");
        std::process::exit(1);
    });
    let current_exe = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("update: install failed: cannot locate the running binary ({e})");
        std::process::exit(1);
    });
    // Keep a backup next to the binary so a bad release is recoverable.
    let backup = current_exe.with_extension("prev");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = std::fs::metadata(&new_bin) {
            let mut perm = md.permissions();
            perm.set_mode(0o755);
            let _ = std::fs::set_permissions(&new_bin, perm);
        }
    }
    if std::fs::copy(&current_exe, &backup).is_err() {
        eprintln!(
            "⚠ could not write a backup next to {}",
            current_exe.display()
        );
    }
    if let Err(e) = std::fs::copy(&new_bin, &current_exe) {
        eprintln!(
            "update: install failed: cannot replace {} ({e}); backup at {}",
            current_exe.display(),
            backup.display()
        );
        std::process::exit(1);
    }
    let _ = std::fs::remove_dir_all(&tmp);

    println!("✓ Pantheon updated to {tag}");
    println!("  Binary: {}", current_exe.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_match_release_workflow() {
        assert_eq!(
            asset_name("v0.1.0", "linux", "x86_64"),
            "pantheon-v0.1.0-linux-x86_64.tar.gz"
        );
        assert_eq!(
            asset_name("v0.1.0", "darwin", "aarch64"),
            "pantheon-v0.1.0-darwin-aarch64.tar.gz"
        );
        assert_eq!(
            asset_name("v0.1.0", "windows", "x86_64"),
            "pantheon-v0.1.0-windows-x86_64.zip"
        );
    }

    #[test]
    fn newer_detection_ignores_leading_v() {
        assert!(is_newer("0.1.0", "v0.2.0"));
        assert!(is_newer("v0.1.0", "v0.1.1"));
        assert!(!is_newer("v0.2.0", "v0.2.0"));
        assert!(!is_newer("v0.2.0", "v0.1.9"));
    }

    #[test]
    fn download_url_shape() {
        assert_eq!(
            download_url("k1ng0mar/pantheon", "v0.1.0", "pantheon-v0.1.0-linux-x86_64.tar.gz"),
            "https://github.com/k1ng0mar/pantheon/releases/download/v0.1.0/pantheon-v0.1.0-linux-x86_64.tar.gz"
        );
    }

    #[test]
    fn checksum_line_matches_accepts_common_formats() {
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        // Plain hex line.
        assert_eq!(checksum_line_matches(hex, hex), Ok(true));
        // sha256sum two-column output.
        assert_eq!(
            checksum_line_matches(&format!("{hex}  pantheon-v0.1.0.tar.gz"), hex),
            Ok(true)
        );
        // BSD `cksum -a sha256` style.
        assert_eq!(
            checksum_line_matches(&format!("SHA256 (pantheon.tar.gz) = {hex}"), hex),
            Ok(true)
        );
        // Case-insensitive.
        assert_eq!(checksum_line_matches(&hex.to_uppercase(), hex), Ok(true));
    }

    #[test]
    fn checksum_line_mismatch_is_an_error() {
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let other = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let err = checksum_line_matches(other, hex).unwrap_err();
        assert!(err.contains("mismatch"), "{err}");
    }

    #[test]
    fn checksum_line_without_digest_is_not_verified() {
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert_eq!(checksum_line_matches("", hex), Ok(false));
        assert_eq!(
            checksum_line_matches("not a checksum file\n", hex),
            Ok(false)
        );
    }
}
