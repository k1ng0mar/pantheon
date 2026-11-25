//! Prepares a cloud machine for a Pantheon run (docs/cloud-agents.md, step 3).
//!
//! Pantheon signs in on this computer by itself, so nothing here copies another
//! agent's login file: a cloud box gets a git identity and, when `gh` is signed
//! in, a scoped GitHub token. Never logs secret values.
use crate::cloud::sandbox::Sandbox;
use crate::Result;
use std::{process::Command, time::Duration};

/// Values the agent process inherits on the cloud machine.
pub(crate) fn push(sandbox: &dyn Sandbox, native: &str) -> Result<Vec<(String, String)>> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(token) = local_gh_token() {
        configure_github_helper(sandbox, native)?;
        env.push(("GH_TOKEN".into(), token));
    }
    if let Some(value) = global_git_config("user.name") {
        env.push(("GIT_AUTHOR_NAME".into(), value.clone()));
        env.push(("GIT_COMMITTER_NAME".into(), value));
    }
    if let Some(value) = global_git_config("user.email") {
        env.push(("GIT_AUTHOR_EMAIL".into(), value.clone()));
        env.push(("GIT_COMMITTER_EMAIL".into(), value));
    }
    Ok(env)
}

fn local_gh_token() -> Option<String> {
    let mut command = Command::new("gh");
    command.args(["auth", "token"]);
    let output = crate::workspace::run_process(
        command,
        "GitHub authentication",
        8192,
        Duration::from_secs(20),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout.bytes)
        .ok()?
        .trim()
        .to_owned();
    (!token.is_empty()).then_some(token)
}

fn configure_github_helper(sandbox: &dyn Sandbox, native: &str) -> Result<()> {
    let helper = "!f() { [ \"$1\" = get ] || exit 0; host=; while IFS= read -r line; do case \"$line\" in host=*) host=${line#host=};; esac; done; [ \"$host\" = github.com ] || exit 0; printf 'username=x-access-token\\npassword=%s\\n' \"$GH_TOKEN\"; }; f";
    let command = format!(
        "git config --global --replace-all credential.helper {}",
        crate::cloud::shell_quote(helper)
    );
    if command.len() >= 64 * 1024 {
        return Err("Cloud GitHub credential helper command exceeded the sandbox limit.".into());
    }
    crate::cloud::sandbox::checked(sandbox.exec(native, &command, Duration::from_secs(30))?)?;
    Ok(())
}

fn global_git_config(key: &str) -> Option<String> {
    let mut command = Command::new("git");
    command.args(["config", "--global", "--get", key]);
    let output =
        crate::workspace::run_process(command, "Git configuration", 8192, Duration::from_secs(20))
            .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout.bytes)
        .ok()?
        .trim()
        .to_owned();
    (!value.is_empty()).then_some(value)
}
