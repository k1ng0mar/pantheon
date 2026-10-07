//! The `pantheon cloudflare` verb: `setup` and `status`.
//!
//! `setup` walks the integration's dependency chain (node, npm, the cf
//! CLI, the API token, the skills import, the config section) and does
//! each part it is allowed to do: it installs what the user confirms,
//! imports Cloudflare's official skills through the existing repo-import
//! path, and writes `[cloudflare]` through the same atomic write the
//! setup wizard uses. It never reads, prints, or stores the token value:
//! the broker resolves it from the environment at call time.
//!
//! `status` reports what is present, authenticated, and enabled, without
//! changing anything.

use pantheon_api::error::PantheonError;
use std::path::{Path, PathBuf};

pub const CF_SKILLS_REPO: &str = "https://github.com/cloudflare/skills";

/// One dependency check: what we looked for and what we found.
pub struct DepCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Detect the node/npm/cf chain with a fake-able detect function so tests
/// run without the tools installed. `detect` returns true when present.
pub fn check_dependencies(detect: &dyn Fn(&str) -> bool) -> Vec<DepCheck> {
    vec![
        DepCheck {
            name: "node".into(),
            ok: detect("command -v node"),
            detail: "needed by npm and the cf CLI".into(),
        },
        DepCheck {
            name: "npm".into(),
            ok: detect("command -v npm"),
            detail: "installs cf (`npm install -g cf`)".into(),
        },
        DepCheck {
            name: "cf".into(),
            ok: detect("command -v cf"),
            detail: "the Cloudflare CLI (`npm install -g cf`)".into(),
        },
    ]
}

/// Read the `[cloudflare]` section from the data dir's config.toml.
/// `load_or_report` takes the data dir and returns None for a missing
/// config (never for a parse error, which it reports and exits on).
fn load_section(data_dir: &Path) -> Option<pantheon_api::config::CloudflareSection> {
    crate::config::Config::load_or_report(data_dir).and_then(|c| c.cloudflare)
}

/// The status report, one row per fact. Shared by `cloudflare status` and
/// the doctor's cloudflare section.
pub struct CloudflareStatus {
    pub deps: Vec<DepCheck>,
    pub token_resolvable: bool,
    pub token_secret_name: String,
    pub section_enabled: bool,
    pub section_present: bool,
    /// `cf auth whoami` output summary when cf is present and the call
    /// succeeds (account name), or the error text when it does not.
    pub auth: Option<Result<String, String>>,
}

/// Assemble the status without spawning `cf` when the binary is missing.
/// `whoami` runs the real CLI when available; `None` means "not checked".
pub fn gather_status(
    data_dir: &Path,
    detect: &dyn Fn(&str) -> bool,
    whoami: &dyn Fn() -> Option<Result<String, String>>,
) -> CloudflareStatus {
    let section = load_section(data_dir);
    let secret_name = section
        .as_ref()
        .and_then(|s| s.api_token_secret.clone())
        .unwrap_or_else(|| "CLOUDFLARE_API_TOKEN".to_string());
    let token_resolvable = std::env::var(&secret_name)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let deps = check_dependencies(detect);
    let cf_present = deps.iter().find(|d| d.name == "cf").map(|d| d.ok);
    let auth = if cf_present == Some(true) {
        whoami()
    } else {
        None
    };
    CloudflareStatus {
        deps,
        token_resolvable,
        token_secret_name: secret_name,
        section_enabled: section.as_ref().map(|s| s.enabled).unwrap_or(false),
        section_present: section.is_some(),
        auth,
    }
}

/// Render the status for humans.
pub fn render_status(s: &CloudflareStatus) -> String {
    let mut out = String::from("cloudflare status\n");
    for d in &s.deps {
        let mark = if d.ok { "ok" } else { "missing" };
        out.push_str(&format!("  {}: {} ({})\n", d.name, mark, d.detail));
    }
    out.push_str(&format!(
        "  token: {} ({})\n",
        if s.token_resolvable {
            "resolvable"
        } else {
            "not found in environment"
        },
        s.token_secret_name
    ));
    out.push_str(&format!(
        "  [cloudflare] section: {}\n",
        if !s.section_present {
            "absent (integration off)"
        } else if s.section_enabled {
            "enabled"
        } else {
            "present but disabled"
        }
    ));
    match &s.auth {
        None => out.push_str("  auth: not checked (cf missing)\n"),
        Some(Ok(a)) => out.push_str(&format!("  auth: {a}\n")),
        Some(Err(e)) => out.push_str(&format!("  auth: FAILED: {e}\n")),
    }
    out
}

/// Write `[cloudflare]` into the data dir's config.toml, preserving every
/// other section. tmp+rename atomic, same shape the setup wizard uses.
pub fn write_config_section(
    data_dir: &Path,
    section: pantheon_api::config::CloudflareSection,
) -> Result<PathBuf, PantheonError> {
    let path = crate::config::Config::path(data_dir);
    let mut cfg = crate::config::Config::load_or_report(data_dir).unwrap_or_default();
    cfg.cloudflare = Some(section);
    let text = toml::to_string_pretty(&cfg).map_err(|e| {
        pantheon_api::error::PantheonError::new(
            "CF_CONFIG_SERIALIZE",
            pantheon_api::error::Layer::Execution,
            false,
            format!("serialize config: {e}"),
            "report this: it means a config field cannot round-trip",
            "",
        )
    })?;
    let tmp = data_dir.join(".config.toml.cf-tmp");
    std::fs::write(&tmp, &text).map_err(|e| {
        pantheon_api::error::PantheonError::new(
            "CF_CONFIG_WRITE",
            pantheon_api::error::Layer::Execution,
            false,
            format!("write {}: {e}", tmp.display()),
            "check the data dir permissions",
            "",
        )
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        pantheon_api::error::PantheonError::new(
            "CF_CONFIG_RENAME",
            pantheon_api::error::Layer::Execution,
            false,
            format!("rename into {}: {e}", path.display()),
            "check the data dir permissions",
            "",
        )
    })?;
    Ok(path)
}

/// Run `cf auth whoami` through the real CLI and summarize the output.
/// The output carries no secret material (cf prints account names and
/// emails), so the summary is safe to show.
pub fn run_whoami() -> Option<Result<String, String>> {
    let out = std::process::Command::new("cf")
        .args(["auth", "whoami"])
        .output()
        .ok()?;
    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        // First account name line, or the authenticated flag.
        let summary = text
            .lines()
            .find(|l| l.contains("\"name\""))
            .map(|l| l.trim().to_string())
            .unwrap_or_else(|| "authenticated".to_string());
        Some(Ok(summary))
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Some(Err(err
            .lines()
            .next()
            .unwrap_or("cf auth failed")
            .to_string()))
    }
}

/// Entry: `pantheon cloudflare setup|status`.
pub fn cmd_cloudflare(args: &[String]) {
    let verb = args.get(2).map(|s| s.as_str()).unwrap_or("status");
    let dd = crate::terminal::data_dir();
    match verb {
        "setup" => cmd_setup(&dd),
        "status" => {
            let status = gather_status(&dd, &crate::setup_providers::detect_binary, &run_whoami);
            print!("{}", render_status(&status));
        }
        "--help" | "-h" | "help" => print_help(),
        other => {
            eprintln!("unknown cloudflare verb: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!("usage: pantheon cloudflare <setup|status>");
    println!("  setup   check deps, offer cf install, import official skills, write [cloudflare]");
    println!("  status  report what is present, authenticated, and enabled");
}

fn cmd_setup(data_dir: &Path) {
    println!("cloudflare setup");
    // 1. Dependencies, install-or-skip for cf.
    let deps = check_dependencies(&crate::setup_providers::detect_binary);
    for d in &deps {
        println!("  {}: {}", d.name, if d.ok { "found" } else { "MISSING" });
    }
    let node_ok = deps.iter().find(|d| d.name == "node").map(|d| d.ok);
    let npm_ok = deps.iter().find(|d| d.name == "npm").map(|d| d.ok);
    let cf_ok = deps.iter().find(|d| d.name == "cf").map(|d| d.ok);
    if node_ok != Some(true) || npm_ok != Some(true) {
        println!("  node and npm are required before the cf CLI can be installed.");
        println!("  install Node.js 22+, then rerun `pantheon cloudflare setup`.");
    } else if cf_ok != Some(true) {
        let ok = crate::prompt::pick_confirm(
            "Cloudflare",
            "Install the cf CLI globally now (`npm install -g cf`)?",
            true,
        )
        .unwrap_or(false);
        if ok {
            println!("  installing cf ...");
            let ok = crate::skill_deps::shell_install("npm install -g cf");
            if ok {
                println!("  cf installed");
            } else {
                println!("  install failed. by hand: npm install -g cf");
            }
        } else {
            println!("  skipped. cf stays missing; setup records the rest anyway.");
        }
    }
    // 2. Token: point at the secret name, never read the value.
    let secret = "CLOUDFLARE_API_TOKEN";
    let have = std::env::var(secret)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if have {
        println!("  {secret}: found in the environment (value never read or printed)");
    } else {
        println!("  {secret}: NOT set.");
        println!("  create a scoped API token in the Cloudflare dashboard");
        println!("  (My Profile > API Tokens), then export {secret} before the");
        println!("  session that uses it. Pantheon reads it only through the");
        println!("  secrets broker at call time.");
    }
    // 3. Skills import through the existing repo path.
    let ok = crate::prompt::pick_confirm(
        "Cloudflare",
        "Import Cloudflare's official skills from github.com/cloudflare/skills?",
        true,
    )
    .unwrap_or(false);
    if ok {
        match pantheon_exec::skills::import_skills_from_repo(
            data_dir,
            CF_SKILLS_REPO,
            Some("skills"),
        ) {
            Ok(items) => {
                for (n, p) in &items {
                    println!("  imported {n} -> {}", p.display());
                }
                println!("  {} skill(s) imported", items.len());
            }
            Err(e) => println!("  skills import failed: {e} (rerun setup to retry)"),
        }
    }
    // 4. Config section: enable when a token resolves, otherwise write
    // the section disabled so the record exists but injects nothing.
    let section = pantheon_api::config::CloudflareSection {
        enabled: have,
        api_token_secret: Some(secret.to_string()),
        account_id: None,
    };
    match write_config_section(data_dir, section) {
        Ok(p) => println!("  wrote [cloudflare] (enabled: {have}) -> {}", p.display()),
        Err(e) => println!("  config write failed: {e}"),
    }
    println!("done. `pantheon cloudflare status` verifies.");
}
