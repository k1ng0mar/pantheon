//! `pantheon desktop` — build (when needed) and launch the desktop app, and
//! register it with the desktop environment so app pickers can open it.
//!
//! The desktop app lives in `<repo>/desktop` (Tauri 2; own workspace, excluded
//! from the std-only one). First run builds: `npm install`, `npm run build`
//! (vite → dist), `cargo build --release` (embeds dist). Later runs reuse the
//! artifacts and only rebuild what is missing or stale. After a successful
//! build the app registers itself into XDG menus
//! (`~/.local/share/applications/pantheon-desktop.desktop` + hicolor icon).
//!
//! Flags: `--rebuild` force the full build; `--skip-build` launch only;
//! `--register` (re)write the desktop entry; `--no-register` skip it.

use crate::args::Args;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// `argv` is the full command line, `pantheon desktop …` at the front.
const DESKTOP_ENTRY_NAME: &str = "pantheon-desktop";

/// `argv` is the full command line, `pantheon desktop …` at the front.
pub fn cmd_desktop(argv: &[String]) -> ! {
    // Help is a question, not a build: without this, `pantheon
    // desktop --help` fell through to ensure_built and tried to
    // build/launch the app. Unknown flags were swallowed the same
    // way (a typo like --rebuid "worked" by doing the default).
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("pantheon desktop [--rebuild] [--skip-build] [--register] [--no-register]");
        println!();
        println!("Build (when needed) and launch the desktop app, and register");
        println!("it with the desktop environment so app pickers can open it.");
        println!("  --rebuild     force the full build");
        println!("  --skip-build  launch only, never build");
        println!("  --register    (re)write the desktop entry and stop (no launch)");
        println!("  --no-register skip the desktop-entry registration");
        std::process::exit(0);
    }
    for a in argv.iter().skip(2) {
        if a.starts_with('-')
            && !matches!(
                a.as_str(),
                "--rebuild" | "--skip-build" | "--register" | "--no-register"
            )
        {
            eprintln!("desktop: unknown flag '{a}'");
            std::process::exit(2);
        }
        if !a.starts_with('-') {
            eprintln!("desktop: unexpected argument '{a}' (this command takes flags only)");
            std::process::exit(2);
        }
    }
    let args = Args::parse(argv);
    let rebuild = args.has("rebuild");
    let skip_build = args.has("skip-build");
    let register = args.has("register");
    let no_register = args.has("no-register");

    let Some(dir) = locate_desktop_dir() else {
        eprintln!(
            "pantheon desktop: cannot find the desktop app. Set PANTHEON_DESKTOP_DIR \
             to the checkout's `desktop/` directory (it lives next to the runtime \
             sources: <repo>/desktop)."
        );
        std::process::exit(2);
    };

    if !skip_build {
        if let Err(error) = ensure_built(&dir, rebuild) {
            eprintln!("pantheon desktop: build failed: {error}");
            std::process::exit(1);
        }
    }

    let binary = dir.join("src-tauri/target/release/pantheon-desktop");
    if !binary.exists() {
        eprintln!(
            "pantheon desktop: no built app at {}. Run `pantheon desktop --rebuild`.",
            binary.display()
        );
        std::process::exit(2);
    }

    // An explicit `--register` is a registration request, not a launch:
    // a failed registration must exit non-zero (the old code printed
    // "registration skipped" and still exited 0, so scripts could not
    // tell). The default path (register-then-launch) keeps the softer
    // behaviour: registration is best-effort there and launch proceeds.
    let mut registration_failed = false;
    if !no_register {
        match register_desktop_entry(&dir, &binary) {
            Ok(entry) => println!("registered: {}", entry.display()),
            Err(error) => {
                eprintln!("pantheon desktop: registration skipped ({error})");
                registration_failed = true;
            }
        }
    }
    if register {
        std::process::exit(if registration_failed { 1 } else { 0 });
    }

    println!("launching Pantheon desktop…");
    let status = Command::new(&binary).status().unwrap_or_else(|error| {
        eprintln!(
            "pantheon desktop: failed to launch {}: {error}",
            binary.display()
        );
        std::process::exit(1);
    });
    std::process::exit(status.code().unwrap_or(1));
}

/// Discovery ladder (plan doc §12: one resolver, validated candidates):
/// explicit env → checkout layout (exe at `<repo>/target/release/pantheon`)
/// → per-user install location.
fn locate_desktop_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PANTHEON_DESKTOP_DIR") {
        let dir = PathBuf::from(dir);
        if dir.join("src-tauri").is_dir() {
            return Some(dir);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        // <repo>/target/{debug,release,fast-release}/pantheon[-desktop] → <repo>/desktop
        if let Some(repo) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            let dir = repo.join("desktop");
            if dir.join("src-tauri").is_dir() {
                return Some(dir.canonicalize().unwrap_or(dir));
            }
        }
    }
    let user = home()?.join(".local/share/pantheon/desktop");
    if user.join("src-tauri").is_dir() {
        return Some(user);
    }
    None
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn ensure_built(dir: &Path, rebuild: bool) -> Result<(), String> {
    let run = |description: &str, mut command: Command| -> Result<(), String> {
        println!("pantheon desktop: {description}…");
        let status = command
            .status()
            .map_err(|e| format!("{description}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{description} exited {status}"))
        }
    };

    if !dir.join("node_modules").exists() || rebuild {
        let mut command = Command::new("npm");
        command.arg("install").current_dir(dir);
        run("installing desktop dependencies (npm install)", command)?;
    }

    let dist = dir.join("dist/index.html");
    if rebuild
        || !dist.exists()
        || stale(&dist, &dir.join("src"))
        || stale(&dist, &dir.join("index.html"))
    {
        let mut command = Command::new("npm");
        command.arg("run").arg("build").current_dir(dir);
        run("building desktop frontend (vite)", command)?;
    }

    let binary = dir.join("src-tauri/target/release/pantheon-desktop");
    if rebuild
        || !binary.exists()
        || stale(&binary, &dir.join("dist"))
        || stale(&binary, &dir.join("src-tauri/src"))
    {
        let mut command = Command::new("cargo");
        // custom-protocol embeds the built frontend; without it the shell
        // bakes the devUrl and the window shows WebKit's connect-refused page.
        command
            .args(["build", "--release", "--features", "custom-protocol"])
            .current_dir(dir.join("src-tauri"));
        run("building desktop shell (cargo build --release)", command)?;
    }
    Ok(())
}

/// `target` is stale when `against` (a file or directory tree) holds anything newer.
fn stale(target: &Path, against: &Path) -> bool {
    let Some(built) = modified(target) else {
        return true;
    };
    newest_modified(against).is_some_and(|newest| newest > built)
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn newest_modified(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_file() {
        return modified(path);
    }
    std::fs::read_dir(path)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| newest_modified(&entry.path()))
        .max()
        .or_else(|| modified(path))
}

/// Write the XDG desktop entry + hicolor icon so app pickers list Pantheon.
fn register_desktop_entry(dir: &Path, binary: &Path) -> Result<PathBuf, String> {
    let home = home().ok_or("no HOME")?;
    let icon_dir = home.join(".local/share/icons/hicolor/1024x1024/apps");
    let icon_source = dir.join("src-tauri/icons/icon.png");
    std::fs::create_dir_all(&icon_dir).map_err(|e| format!("icon dir: {e}"))?;
    let icon = icon_dir.join("pantheon.png");
    std::fs::copy(&icon_source, &icon).map_err(|e| format!("icon copy: {e}"))?;

    let entry = format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Pantheon\nGenericName=AI Agent Desktop\nComment=Pantheon — the durable runtime for AI agents\nExec={}\nIcon={}\nTerminal=false\nCategories=Development;IDE;\nStartupWMClass=pantheon-desktop\nStartupNotify=true\n",
        binary.display(),
        icon.display(),
    );
    let applications = home.join(".local/share/applications");
    std::fs::create_dir_all(&applications).map_err(|e| format!("applications dir: {e}"))?;
    let path = applications.join(format!("{DESKTOP_ENTRY_NAME}.desktop"));
    std::fs::write(&path, entry).map_err(|e| format!("write entry: {e}"))?;
    // Menu caches pick up user entries without this on most desktops; run it
    // when present and ignore failure (best effort, never blocks launch).
    let _ = Command::new("update-desktop-database")
        .arg(&applications)
        .status();
    Ok(path)
}
