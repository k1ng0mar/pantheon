//! The release asset naming contract, checked as data.
//!
//! Three parties construct the same download name: `install.sh`,
//! `install.ps1`, and `pantheon update`. The release workflow uploads assets
//! built from its own matrix. `release.yml` calls the names a contract and
//! warns that renaming an asset breaks the installer on every machine, but
//! until this file existed nothing verified the two sides agreed - a drift
//! would surface only on a user's first install.
//!
//! These tests read the repository as text, so they need no binary and no
//! network. See `docs/developer/release-asset-contract.md`.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/eval for this crate.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("eval has a parent")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// One `os`/`arch` pair the release workflow publishes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Target {
    os: String,
    arch: String,
}

/// Every `os:` / `arch:` pair in the release build matrix.
///
/// The matrix is a list of entries each carrying `target`, `os`, `arch`, and
/// `runner`; `os` and `arch` are what end up in the asset name, so those are
/// what this parses.
fn release_matrix() -> Vec<Target> {
    let yml = read(".github/workflows/release.yml");
    let mut out = Vec::new();
    let mut cur_os: Option<String> = None;
    for line in yml.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("os:") {
            cur_os = Some(rest.trim().trim_matches('"').to_string());
        } else if let Some(rest) = t.strip_prefix("arch:") {
            let arch = rest.trim().trim_matches('"').to_string();
            if let Some(os) = cur_os.take() {
                out.push(Target { os, arch });
            }
        }
    }
    out.sort();
    out
}

/// The asset stem `install.sh` builds: `pantheon-${VERSION}-${OS_ID}-${ARCH_ID}`.
fn sh_stem_template() -> String {
    let sh = read("install.sh");
    let line = sh
        .lines()
        .find(|l| l.contains("ASSET=\"pantheon-"))
        .expect("install.sh builds an ASSET name");
    let inner = line
        .split("ASSET=\"")
        .nth(1)
        .expect("ASSET assignment")
        .split('"')
        .next()
        .expect("closing quote");
    // Strip the archive suffix. Splitting on '.' would wrongly cut
    // "...tar.gz" down to "...tar", so strip the known suffixes explicitly.
    for suffix in [".tar.gz", ".tgz", ".zip"] {
        if let Some(stem) = inner.strip_suffix(suffix) {
            return stem.to_string();
        }
    }
    inner.to_string()
}

/// The `case` arms that map a detected OS/arch to the ids used in the name.
///
/// The arms read `Linux) OS_ID="linux" ;;`, so the value is the right-hand
/// side with the `NAME=` prefix and quoting removed.
fn sh_mapped_id(case_header: &str) -> Vec<String> {
    let sh = read("install.sh");
    let mut out = Vec::new();
    let mut in_block = false;
    for line in sh.lines() {
        if line.contains(case_header) {
            in_block = true;
            continue;
        }
        if in_block {
            if line.trim() == "esac" {
                break;
            }
            let Some((_, rhs)) = line.split_once(')') else {
                continue;
            };
            let value = rhs.trim().trim_end_matches(";;").trim();
            // Drop an `OS_ID=` / `ARCH_ID=` assignment prefix if present.
            let value = match value.split_once('=') {
                Some((_, v)) => v.trim(),
                None => value,
            };
            let value = value.trim_matches('"');
            if !value.is_empty() && !value.starts_with("fail") && !value.contains('$') {
                out.push(value.to_string());
            }
        }
    }
    out
}

#[test]
fn install_sh_uses_the_tag_os_arch_shape() {
    let stem = sh_stem_template();
    assert_eq!(
        stem, "pantheon-${VERSION}-${OS_ID}-${ARCH_ID}",
        "install.sh asset stem must stay pantheon-<tag>-<os>-<arch>; got {stem}"
    );
}

#[test]
fn install_ps1_uses_the_tag_os_arch_shape() {
    let ps1 = read("install.ps1");
    let line = ps1
        .lines()
        .find(|l| l.contains("pantheon-$tag-windows-$archId"))
        .or_else(|| ps1.lines().find(|l| l.contains("$asset = \"pantheon-")))
        .expect("install.ps1 builds an asset name");
    assert!(
        line.contains("windows"),
        "install.ps1 must request the windows asset: {line}"
    );
    assert!(
        line.contains("$archId"),
        "install.ps1 must interpolate the arch id: {line}"
    );
    assert!(
        line.contains(".zip"),
        "install.ps1 must request the .zip asset (windows packaging): {line}"
    );
}

#[test]
fn release_matrix_is_not_empty_and_covers_both_archive_kinds() {
    let targets = release_matrix();
    assert!(
        targets.len() >= 4,
        "expected the full release matrix, parsed {targets:?}"
    );
    assert!(
        targets.iter().any(|t| t.os == "windows"),
        "a windows target must exist for install.ps1 to fetch: {targets:?}"
    );
    assert!(
        targets.iter().any(|t| t.os == "linux"),
        "a linux target must exist: {targets:?}"
    );
    assert!(
        targets.iter().any(|t| t.os == "darwin"),
        "a darwin target must exist: {targets:?}"
    );
}

/// The core assertion: for every os/arch the release publishes, both
/// installers can name it. An installer that maps a platform to an id the
/// release never uploads is a 404 on a user's first install.
#[test]
fn every_release_target_is_nameable_by_the_installers() {
    let targets = release_matrix();
    let sh_os = sh_mapped_id("case \"$OS\"");
    let sh_arch = sh_mapped_id("case \"$ARCH\"");

    assert!(
        !sh_os.is_empty(),
        "install.sh must map uname output to an OS id"
    );
    assert!(
        !sh_arch.is_empty(),
        "install.sh must map uname -m output to an arch id"
    );

    for t in &targets {
        // install.sh only ever fetches tarballs; windows is install.ps1's job.
        if t.os != "windows" {
            assert!(
                sh_os.contains(&t.os),
                "release publishes os {:?} but install.sh has no case arm for it ({sh_os:?})",
                t.os
            );
            assert!(
                sh_arch.iter().any(|a| a == &t.arch),
                "release publishes arch {:?} but install.sh has no case arm for it ({sh_arch:?})",
                t.arch
            );
        }
    }
}

/// The reverse direction: an installer arm for a platform the release never
/// publishes is a dead path that will 404. Both directions matter, because
/// the two files are edited independently.
#[test]
fn every_installer_platform_is_published_by_the_release() {
    let targets = release_matrix();
    let sh_os = sh_mapped_id("case \"$OS\"");
    for os in &sh_os {
        assert!(
            targets.iter().any(|t| &t.os == os),
            "install.sh maps to os {os:?} but the release publishes {targets:?}"
        );
    }
    let sh_arch = sh_mapped_id("case \"$ARCH\"");
    for arch in &sh_arch {
        assert!(
            targets.iter().any(|t| &t.arch == arch),
            "install.sh maps to arch {arch:?} but the release publishes {targets:?}"
        );
    }
}

/// `pantheon update` reads the same names through the helper. It resolves the
/// asset from the running binary's own target triple, so the tag/os/arch
/// vocabulary must match what the release publishes - asserted here at the
/// format level, since the runtime name is built in Rust.
#[test]
fn updater_asset_format_matches_the_release_names() {
    let src = read("crates/pantheon-helper/src/main.rs");
    let has_prefix = src.contains("pantheon-");
    assert!(
        has_prefix,
        "the updater must build asset names from the same pantheon- prefix"
    );
    for needle in ["tar.gz", "zip"] {
        assert!(
            src.contains(needle),
            "the updater must know the {needle} archive kind the release publishes"
        );
    }
}
