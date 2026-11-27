//! Installs the agent runtime on a cloud machine (docs/cloud-agents.md, step 2).
use crate::cloud::sandbox::Sandbox;
use crate::Result;
use std::time::Duration;

/// The only agent runtime this app runs, on this computer or on a machine.
pub(crate) const HARNESSES: [&str; 1] = ["pantheon"];
/// Where a machine keeps the uploaded runtime.
const REMOTE_BINARY: &str = ".pantheon/bin/pantheon";

/// Makes sure Pantheon can run on the machine; returns the absolute path to
/// execute there. The desktop uploads the same binary it runs locally, so a
/// cloud machine never downloads a second release or depends on npm.
pub(crate) fn ensure_runtime(
    sandbox: &dyn Sandbox,
    native: &str,
    harness: &str,
    progress: &dyn Fn(&str),
) -> Result<String> {
    if !HARNESSES.contains(&harness) {
        return Err(format!(
            "Unsupported cloud agent '{harness}'. Supported agents: {}.",
            HARNESSES.join(", ")
        ));
    }
    let marker = format!("$HOME/.pantheon/runtime/{harness}");
    let cached = execute(
        sandbox,
        native,
        &format!(
            r#"set -eu
marker="{marker}"
if [ -s "$marker" ]; then
  program=$(sed -n '1p' "$marker")
  if [ "${{program#/}}" != "$program" ] && [ -x "$program" ] && "$program" --version >/dev/null 2>&1; then
    printf '%s\n' "$program"
  fi
fi
"#
        ),
    )?;
    if let Some(path) = cached.lines().last().filter(|line| line.starts_with('/')) {
        return Ok(path.to_owned());
    }

    let (_arch, _libc) = detect_linux(sandbox, native)?;
    ensure_tools(sandbox, native, &["git", "curl", "tar"], progress)?;
    let local = crate::providers::executable("pantheon")
        .ok_or("Install Pantheon on this computer before running a task on a machine.")?;
    let bytes = std::fs::read(&local).map_err(|error| {
        format!(
            "Could not read {} to send it to the machine: {error}",
            local.display()
        )
    })?;
    // The machine is Linux; a non-Linux build would only fail once it ran.
    if !bytes.starts_with(b"\x7fELF") {
        return Err(
            "This computer's Pantheon build cannot run on a Linux machine. Install a Linux build, or run this task here."
                .into(),
        );
    }
    progress("Sending Pantheon to the machine…");
    execute(sandbox, native, "set -eu\nmkdir -p \"$HOME/.pantheon/bin\"")?;
    sandbox.upload(native, REMOTE_BINARY, &bytes, false)?;
    execute(
        sandbox,
        native,
        "set -eu\nchmod 755 \"$HOME/.pantheon/bin/pantheon\"\n\"$HOME/.pantheon/bin/pantheon\" --version >/dev/null",
    )?;
    mark_runtime(sandbox, native, harness, "pantheon")
}

fn execute(sandbox: &dyn Sandbox, native: &str, command: &str) -> Result<String> {
    if command.len() >= 64 * 1024 {
        return Err("Cloud runtime setup command exceeded the sandbox limit.".into());
    }
    let (code, output) = sandbox.exec(native, command, Duration::from_secs(900))?;
    crate::cloud::sandbox::checked((code, output.clone()))?;
    Ok(output)
}

fn detect_linux(sandbox: &dyn Sandbox, native: &str) -> Result<(String, String)> {
    let output = execute(
        sandbox,
        native,
        r#"set -eu
[ "$(uname -s)" = Linux ] || { echo 'Cloud agent runtimes currently require Linux.' >&2; exit 1; }
case "$(uname -m)" in
  x86_64|amd64) arch=x64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo 'Cloud agent runtimes support x86_64 and arm64 Linux machines.' >&2; exit 1 ;;
esac
if [ -f /etc/alpine-release ] || (ldd --version 2>&1 | grep -qi musl); then libc=musl; else libc=glibc; fi
printf '%s %s\n' "$arch" "$libc"
"#,
    )?;
    let mut fields = output.split_whitespace();
    let arch = fields.next().unwrap_or_default();
    let libc = fields.next().unwrap_or_default();
    if !matches!(arch, "x64" | "arm64") || !matches!(libc, "glibc" | "musl") {
        return Err("Could not identify the cloud machine's Linux architecture.".into());
    }
    Ok((arch.to_owned(), libc.to_owned()))
}

fn ensure_tools(
    sandbox: &dyn Sandbox,
    native: &str,
    tools: &[&str],
    progress: &dyn Fn(&str),
) -> Result<()> {
    let checks = tools
        .iter()
        .map(|tool| {
            format!(
                "command -v {} >/dev/null 2>&1 || printf '%s\\n' {}",
                crate::cloud::shell_quote(tool),
                crate::cloud::shell_quote(tool)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let missing = execute(sandbox, native, &checks)?;
    let missing = missing.split_whitespace().collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    progress("Installing system tools…");
    let apt = package_names(&missing, "apt-get")?;
    let apk = package_names(&missing, "apk")?;
    let dnf = package_names(&missing, "dnf")?;
    let command = format!(
        r#"set -eu
as_root() {{ if [ "$(id -u)" -eq 0 ]; then "$@"; else command -v sudo >/dev/null 2>&1 || {{ echo 'Install the required tools as root or configure passwordless sudo.' >&2; return 1; }}; sudo -n "$@"; fi; }}
if command -v apt-get >/dev/null 2>&1; then as_root apt-get update -y; as_root apt-get install -y {apt}
elif command -v apk >/dev/null 2>&1; then as_root apk add --no-cache {apk}
elif command -v dnf >/dev/null 2>&1; then as_root dnf install -y {dnf}
else echo 'Install the missing cloud agent tools with apt-get, apk, or dnf.' >&2; exit 1
fi
"#
    );
    execute(sandbox, native, &command)?;
    Ok(())
}

fn package_names(tools: &[&str], manager: &str) -> Result<String> {
    tools
        .iter()
        .map(|tool| match (*tool, manager) {
            ("git", _) => Ok("git"),
            ("curl", _) => Ok("curl"),
            ("tar", _) => Ok("tar"),
            ("bash", _) => Ok("bash"),
            ("xz", "apt-get") => Ok("xz-utils"),
            ("xz", _) => Ok("xz"),
            _ => Err("Unknown cloud runtime dependency.".into()),
        })
        .collect::<Result<Vec<_>>>()
        .map(|packages| packages.join(" "))
}

fn mark_runtime(sandbox: &dyn Sandbox, native: &str, harness: &str, cli: &str) -> Result<String> {
    let command = format!(
        r#"set -eu
home=$(cd "$HOME" && pwd -P)
export PATH="$home/.pantheon/bin:$home/.local/bin:$PATH"
resolve() {{
  name=$1
  for dir in "$home/.pantheon/bin" "$home/.local/bin" /usr/local/bin /usr/bin /bin; do
    if [ -x "$dir/$name" ]; then printf '%s\n' "$dir/$name"; return 0; fi
  done
  found=$(command -v "$name" || true)
  case "$found" in /*) printf '%s\n' "$found" ;; *) return 1 ;; esac
}}
program=$(resolve {}) || {{ echo 'Pantheon was not found on the machine.' >&2; exit 1; }}
"$program" --version >/dev/null 2>&1 || {{ echo 'The Pantheon binary on the machine did not pass its version check.' >&2; exit 1; }}
version=$("$program" --version 2>&1 | sed -n '1p')
[ -n "$version" ] || {{ echo 'The Pantheon binary on the machine did not return a version.' >&2; exit 1; }}
mkdir -p "$home/.pantheon/runtime"
tmp="$home/.pantheon/runtime/.{harness}.$$"
printf '%s\n%s\n' "$program" "$version" > "$tmp"
chmod 600 "$tmp"
mv "$tmp" "$home/.pantheon/runtime/{harness}"
printf '%s\n' "$program"
"#,
        crate::cloud::shell_quote(cli)
    );
    let output = execute(sandbox, native, &command)?;
    let path = output.lines().last().unwrap_or_default();
    if !path.starts_with('/') {
        return Err("The cloud runtime path was not absolute.".into());
    }
    Ok(path.to_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::cloud::{sandbox::Info, Machine};
    use std::{
        cell::RefCell,
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("pantheon-setup-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct FakeSandbox {
        root: PathBuf,
        calls: AtomicUsize,
    }
    impl Sandbox for FakeSandbox {
        fn info(&self) -> Info {
            Info {
                persistent: true,
                max_unattended_minutes: 60,
            }
        }
        fn list(&self) -> Result<Vec<Machine>> {
            Ok(Vec::new())
        }
        fn create(&self, _name: &str) -> Result<Machine> {
            Err("unused".into())
        }
        fn power(&self, _native: &str, _action: &str) -> Result<()> {
            Ok(())
        }
        fn exec(
            &self,
            _native: &str,
            command: &str,
            _timeout: Duration,
        ) -> Result<(Option<i32>, String)> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert!(command.len() < 64 * 1024);
            let output = Command::new("sh")
                .args(["-c", command])
                .current_dir(&self.root)
                .env("HOME", self.root.join("home"))
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        self.root.join("bin").display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .output()
                .map_err(|e| e.to_string())?;
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Ok((output.status.code(), text))
        }
        fn keep_alive(&self, _native: &str, _minutes: u32) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn runtime_marker_with_working_cli_skips_installation() {
        let temp = TempDir::new();
        let root = temp.0.join("machine");
        fs::create_dir_all(root.join("home/.pantheon/runtime")).unwrap();
        fs::create_dir_all(root.join("home/.pantheon/bin")).unwrap();
        let executable = root.join("home/.pantheon/bin/pantheon");
        fs::write(&executable, "#!/bin/sh\nprintf 'pantheon 1.0.0\\n'\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        fs::write(
            root.join("home/.pantheon/runtime/pantheon"),
            executable.to_string_lossy().as_bytes(),
        )
        .unwrap();

        let sandbox = FakeSandbox {
            root,
            calls: AtomicUsize::new(0),
        };
        let progress = RefCell::new(Vec::new());
        let path = ensure_runtime(&sandbox, "fake", "pantheon", &|label| {
            progress.borrow_mut().push(label.to_owned())
        })
        .unwrap();

        assert_eq!(path, executable.to_string_lossy());
        assert_eq!(sandbox.calls.load(Ordering::Relaxed), 1);
        assert!(progress.borrow().is_empty());
    }
}
