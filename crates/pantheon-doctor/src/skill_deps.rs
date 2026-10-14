//! Skill dependencies: third-party binaries and packages the bundled
//! skill library needs, surfaced at `pantheon setup` (install-or-skip)
//! and reported by `pantheon doctor` (warn, never fail).
//!
//! The registry is static: one row per dependency, detected by shelling
//! out read-only (`sh -c <detect_cmd>`, see `setup_providers::detect_binary`
//! for the timeout semantics). Nothing here installs silently - the
//! install path only runs after an explicit confirm from the user, and a
//! skip is recorded in `[skill_deps].skipped` (pantheon-api
//! `SkillDepsSection`) so doctor can converge on it later.

use std::process::Command;

/// One dependency the skill library needs.
pub struct SkillDep {
    /// Stable id recorded in `[skill_deps].skipped`.
    pub id: &'static str,
    /// Human name shown in setup and doctor output.
    pub name: &'static str,
    /// How to notice the dependency is present. Read-only, short.
    pub detect_cmd: &'static str,
    /// What to run when the user says install. `None` = advise only.
    pub install_cmd: Option<&'static str>,
    /// Shown when there is no scripted install (or the install failed).
    pub install_hint: &'static str,
    /// Which bundled skill(s) need it, for the doctor warn line.
    pub needed_by: &'static str,
    /// True when the dependency is a Python package and doctor should
    /// also check that `pip` itself is usable.
    pub needs_pip: bool,
}

/// The registry. Sorted by id so doctor output is stable.
pub fn skill_deps() -> &'static [SkillDep] {
    &[
        SkillDep {
            id: "debugpy",
            name: "debugpy",
            detect_cmd: "python3 -m debugpy --version",
            install_cmd: Some("python3 -m pip install --user debugpy"),
            install_hint: "Install with `python3 -m pip install --user debugpy`.",
            needed_by: "python-debug",
            needs_pip: true,
        },
        SkillDep {
            id: "himalaya",
            name: "himalaya",
            detect_cmd: "command -v himalaya",
            install_cmd: None,
            install_hint: "Install the himalaya CLI (github.com/soywod/himalaya), \
                           then run `himalaya account configure`.",
            needed_by: "himalaya-imap",
            needs_pip: false,
        },
        SkillDep {
            id: "node",
            name: "node",
            detect_cmd: "command -v node",
            install_cmd: None,
            install_hint: "Install Node.js from nodejs.org or your package manager.",
            needed_by: "node-debug",
            needs_pip: false,
        },
        SkillDep {
            id: "python3",
            name: "python3",
            detect_cmd: "command -v python3",
            install_cmd: None,
            install_hint: "Install Python 3 from python.org or your package manager.",
            needed_by: "python-debug",
            needs_pip: false,
        },
    ]
}

/// Look one dependency up by stable id (doctor's stale-record hygiene).
pub fn find_dep(id: &str) -> Option<&'static SkillDep> {
    skill_deps().iter().find(|d| d.id == id)
}

/// True when `pip` can run (needed before a `pip install` dep is offered).
pub fn pip_ready(detect: &dyn Fn(&str) -> bool) -> bool {
    detect("python3 -m pip --version")
}

/// Detect every dependency and return the ids that are missing AND
/// skipped by the user. The scripted (`--yes`) path passes
/// `offer_install = None`-shaped logic here: detect only, record the
/// missing ids as skipped, never install behind the user's back.
pub fn detect_all_skipped(detect: &dyn Fn(&str) -> bool) -> Vec<String> {
    skill_deps()
        .iter()
        .filter(|d| !detect(d.detect_cmd))
        .map(|d| d.id.to_string())
        .collect()
}

/// The interactive install-or-skip screen. One status line per dep
/// (present / missing + which skill needs it), then for each missing
/// dep: offer the install, run it through `install` on confirm, and
/// record the id as skipped on decline or failure.
/// Returns the skipped ids for `[skill_deps].skipped`.
pub fn run_skill_deps_screen(
    say: &mut dyn FnMut(&str),
    confirm: &mut dyn FnMut(&str, bool) -> Option<bool>,
    install: &mut dyn FnMut(&str) -> bool,
    detect: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut skipped = Vec::new();
    for dep in skill_deps() {
        if detect(dep.detect_cmd) {
            say(&format!("{}: found", dep.name));
            continue;
        }
        say(&format!(
            "{}: missing - needed by {}",
            dep.name, dep.needed_by
        ));
        let wants = confirm(&format!("Install {} now?", dep.name), true);
        let installed = match wants {
            Some(true) => match dep.install_cmd {
                Some(cmd) => {
                    say(&format!("installing {} ...", dep.name));
                    let ok = install(cmd);
                    if !ok {
                        say(&format!("install failed - {}", dep.install_hint));
                    }
                    ok
                }
                None => {
                    say(&format!("no scripted install: {}", dep.install_hint));
                    false
                }
            },
            // None = user aborted the screen: treat like a skip, not a
            // crash. Every remaining undetected dep is recorded skipped.
            Some(false) | None => false,
        };
        if !installed {
            say(&format!("recorded {} as skipped", dep.id));
            skipped.push(dep.id.to_string());
        } else if !detect(dep.detect_cmd) {
            // The installer reported success but detection still fails:
            // record the skip so doctor reports the real state later
            // instead of pretending the dep exists.
            say(&format!(
                "installed but {} still not detected - recorded as skipped",
                dep.id
            ));
            skipped.push(dep.id.to_string());
        }
    }
    skipped
}

/// The default install runner: shell out, surface nothing on success.
pub fn shell_install(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect_true(_: &str) -> bool {
        true
    }
    fn detect_false(_: &str) -> bool {
        false
    }

    #[test]
    fn registry_is_sorted_and_ids_unique() {
        let deps = skill_deps();
        let mut ids: Vec<&str> = deps.iter().map(|d| d.id).collect();
        let sorted = ids.clone();
        ids.sort_unstable();
        assert_eq!(ids, sorted, "registry must stay sorted by id");
        let unique = ids.clone();
        ids.dedup();
        assert_eq!(ids, unique, "ids must be unique");
    }

    #[test]
    fn every_needed_by_names_a_real_skill_file() {
        // The needed_by value names a bundled skill directory; a typo
        // here makes doctor's warn line point at a skill that is gone.
        for dep in skill_deps() {
            let dir = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../pantheon-exec/bundled-skills"
            );
            let skill = std::path::Path::new(dir).join(dep.needed_by);
            assert!(
                skill.join("SKILL.md").exists(),
                "{} points at missing bundled skill {}",
                dep.id,
                dep.needed_by
            );
        }
    }

    #[test]
    fn find_dep_round_trips_and_misses_cleanly() {
        assert!(find_dep("himalaya").is_some());
        assert!(find_dep("no-such-dep").is_none());
    }

    #[test]
    fn detect_all_skipped_lists_only_missing() {
        // All present -> nothing skipped; none present -> all ids.
        assert!(detect_all_skipped(&detect_true).is_empty());
        let skipped = detect_all_skipped(&detect_false);
        let ids: Vec<&str> = skill_deps().iter().map(|d| d.id).collect();
        assert_eq!(skipped, ids);
    }

    #[test]
    fn screen_skips_when_user_declines_install() {
        let mut lines: Vec<String> = Vec::new();
        let mut say = |l: &str| lines.push(l.to_string());
        let mut confirm = |_: &str, _: bool| Some(false);
        let mut install = |_: &str| {
            panic!("install must not run when the user declines");
        };
        let skipped = run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_false);
        let ids: Vec<&str> = skill_deps().iter().map(|d| d.id).collect();
        assert_eq!(skipped, ids);
        assert!(lines
            .iter()
            .any(|l| l.contains("recorded himalaya as skipped")));
    }

    #[test]
    fn screen_records_skip_when_install_fails() {
        let mut say = |_: &str| {};
        let mut confirm = |_: &str, _: bool| Some(true);
        let mut install = |_: &str| false;
        let skipped = run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_false);
        assert_eq!(skipped.len(), skill_deps().len());
    }

    #[test]
    fn screen_silent_when_everything_present() {
        let mut say = |_: &str| {};
        let mut confirm =
            |_: &str, _: bool| -> Option<bool> { panic!("no confirm when all present") };
        let mut install = |_: &str| -> bool { panic!("no install when all present") };
        let skipped = run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_true);
        assert!(skipped.is_empty());
    }

    #[test]
    fn screen_records_skip_when_install_succeeds_but_detection_still_fails() {
        let mut say = |_: &str| {};
        let mut confirm = |_: &str, _: bool| Some(true);
        let mut install = |_: &str| true;
        let skipped = run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_false);
        assert_eq!(skipped.len(), skill_deps().len());
    }

    #[test]
    fn screen_treats_aborted_confirm_as_skip_not_install() {
        let mut say = |_: &str| {};
        let mut confirm = |_: &str, _: bool| None;
        let mut install = |_: &str| -> bool { panic!("aborted screen must not install") };
        let skipped = run_skill_deps_screen(&mut say, &mut confirm, &mut install, &detect_false);
        assert_eq!(skipped.len(), skill_deps().len());
    }

    #[test]
    fn shell_install_survives_a_missing_shell() {
        assert!(!shell_install("definitely-not-a-real-command-xyz"));
    }
}
