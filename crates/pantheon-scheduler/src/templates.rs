//! Schedule templates: built-in job blueprints plus a user-extensible store.
//!
//! A template is a named, documented starting point for `pantheon schedule
//! create --template <name>`: a default schedule, a prompt with
//! `{{variable}}` placeholders, and the questions needed to fill them.
//! Built-ins are embedded below; users add or override templates with TOML
//! files in `<data_dir>/templates/*.toml`:
//!
//! ```toml
//! name = "my-watch"
//! description = "Watch something I care about."
//! schedule_every = "1h"          # or: schedule_cron = "0 9 * * *"
//! prompt = "Check {{thing}} and report back."
//!
//! [[vars]]
//! name = "thing"
//! question = "What should I watch?"
//! default = "the build"          # optional
//! ```
//!
//! A user file with the same `name` as a built-in replaces it; anything
//! else is added. Broken files are skipped (create-time validation still
//! rejects a bad schedule loudly).

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One fill-in variable in a template prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateVar {
    pub name: String,
    pub question: String,
    pub default: Option<String>,
}

/// A template's default schedule, in `schedule create` flag terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateSchedule {
    /// `--every <duration>`, e.g. `"30m"`.
    Every(String),
    /// `--cron '<expr>'`, e.g. `"0 7 * * *"`.
    Cron(String),
}

/// A schedule blueprint: what to run, when, and what it needs to know.
#[derive(Debug, Clone)]
pub struct ScheduleTemplate {
    pub name: String,
    pub description: String,
    pub schedule: TemplateSchedule,
    pub prompt: String,
    pub vars: Vec<TemplateVar>,
}

fn var(name: &str, question: &str, default: Option<&str>) -> TemplateVar {
    TemplateVar {
        name: name.to_string(),
        question: question.to_string(),
        default: default.map(str::to_string),
    }
}

/// The embedded built-ins. Deliberately few and sharp: each one is
/// something a working developer would genuinely schedule. No wellness
/// nudges, no habit trackers.
pub fn builtin_templates() -> Vec<ScheduleTemplate> {
    vec![
        ScheduleTemplate {
            name: "morning-briefing".into(),
            description: "Overnight digest: GitHub notifications, AI news on a topic, today's calendar."
                .into(),
            schedule: TemplateSchedule::Cron("0 7 * * *".into()),
            prompt: "Write my morning briefing, three sections, tight:\n\n\
                1. GitHub — notifications and activity from the last ~12h on my repos: new issues, \
                PRs needing review, failing CI. Use `gh` (gh api notifications, gh run list).\n\
                2. {{topic}} news — what happened in the last 24h: releases, papers, notable launches \
                and discussions. Search the web; link sources.\n\
                3. Today — what's on the calendar: meetings, deadlines, focus blocks. Use the \
                connected calendar tools.\n\n\
                End with a 3-bullet \"today's priorities\" distilled from all three. \
                Under 400 words total. No preamble.".into(),
            vars: vec![var(
                "topic",
                "Which topic should the AI-news section track?",
                Some("AI agents"),
            )],
        },
        ScheduleTemplate {
            name: "inbox-triage".into(),
            description: "Unread messages across connected channels, summarized with suggested actions."
                .into(),
            schedule: TemplateSchedule::Every("2h".into()),
            prompt: "Triage unread messages across {{channels}} from the last few hours. For each \
                thread with unread activity: one line on what it is, who it's from, and a suggested \
                action — draft the reply, defer, or ignore. Skip newsletters, CI bots, and automated \
                noise entirely. End with a prioritized action list, most urgent first. If there's \
                genuinely nothing new, say so in one line and stop.".into(),
            vars: vec![var(
                "channels",
                "Which channels should I check (comma-separated)?",
                Some("telegram"),
            )],
        },
        ScheduleTemplate {
            name: "repo-watch".into(),
            description: "{{repo}}: failing CI, open PRs/issues, stale branches.".into(),
            schedule: TemplateSchedule::Every("6h".into()),
            prompt: "Watch {{repo}} for problems. Check: CI status on the default branch and open \
                PRs (failing checks first), open PRs and issues sorted by staleness, and branches \
                untouched for 30+ days that look merged or abandoned. Use `gh` for GitHub remotes, \
                git locally. Report as: RED — needs action / YELLOW — watch / GREEN — healthy. \
                One line per item, no fluff.".into(),
            vars: vec![var(
                "repo",
                "Which repo? (owner/name for GitHub, or a local path)",
                None,
            )],
        },
        ScheduleTemplate {
            name: "dep-audit".into(),
            description: "Weekly vulnerable/outdated dependency scan.".into(),
            schedule: TemplateSchedule::Cron("0 9 * * 1".into()),
            prompt: "Audit dependencies in {{path}}. Detect the project type from lockfiles and run \
                the right audit: `cargo audit` (Rust), `npm audit` (Node), `pip-audit` (Python), or \
                the ecosystem equivalent. Report: critical/high vulnerabilities with CVE id and fixed \
                version, notably outdated direct dependencies worth upgrading, and a suggested upgrade \
                order. Skip dev-only low-severity noise unless the fix is trivial. If no audit tool \
                is available for the project type, say which one to install and stop.".into(),
            vars: vec![var(
                "path",
                "Which project directory should I scan?",
                Some("."),
            )],
        },
        ScheduleTemplate {
            name: "weekly-review".into(),
            description: "Pantheon-native: the week's runs, costs, and what shipped, read from the ledger."
                .into(),
            schedule: TemplateSchedule::Cron("0 8 * * 1".into()),
            prompt: "Write my weekly review from the Pantheon ledger. Gather data with \
                `pantheon stats --week --json` and the run history: total runs, success vs failure \
                rate, token and cost totals by model, the most expensive runs, and what shipped \
                (completed runs with real output). Then: biggest win, biggest time sink, one concrete \
                thing to change next week. Honest and specific — cite run ids and numbers, no filler."
                .into(),
            vars: vec![],
        },
        ScheduleTemplate {
            name: "cost-report".into(),
            description: "Spend by model/project over a range, with anomaly flags.".into(),
            schedule: TemplateSchedule::Cron("0 9 * * 1".into()),
            prompt: "Report Pantheon spend over the last {{range}}. Run `pantheon stats --json` for \
                the range (use --week for 7d, --month for 30d): total cost in USD, breakdown by model \
                and by project, per-run cost outliers, and trend vs the previous equal-length period \
                if the data exists. Flag anything anomalous — a single run at 10x the median cost, a \
                new expensive model appearing, a project suddenly burning tokens. Concrete numbers, \
                no padding.".into(),
            vars: vec![var("range", "Over what range?", Some("7d"))],
        },
        ScheduleTemplate {
            name: "gmail-monitor".into(),
            description: "Watch Gmail for a search query, summarize matching new mail."
                .into(),
            schedule: TemplateSchedule::Every("30m".into()),
            prompt: "Watch Gmail for new mail matching: {{query}}. Use the Gmail skill: search for \
                unread messages matching the query from the last check window, and summarize each \
                hit — sender, subject, one-line gist, and why it matched. End with suggested actions \
                for anything needing a response. If nothing new matches, say so in one line and stop. \
                This job is usually created with a --deliver target so hits reach me.".into(),
            vars: vec![var(
                "query",
                "What Gmail search query? (e.g. from:bank subject:alert)",
                None,
            )],
        },
        ScheduleTemplate {
            name: "cost-watch".into(),
            description: "Watch an item's price; alert on moves ≥ threshold or a target cross."
                .into(),
            schedule: TemplateSchedule::Every("6h".into()),
            prompt: "Check the current price of {{item}} at {{url_or_source}}. Use web search/fetch \
                tools; if the page needs JS rendering, say so and stop rather than guessing. Keep a \
                state file at $PANTHEON_DATA_DIR/price-watch.json mapping item names to last-seen \
                prices (create it if absent). Alert — with old price, new price, and % change — when \
                the price moved ≥ {{threshold_pct}}% since the last check, or when it crosses \
                {{target_price}} in either direction (ignore the target condition when blank). If \
                neither fired, reply in one line with the current price and stop — no noise.".into(),
            vars: vec![
                var("item", "What item should I watch the price of?", None),
                var(
                    "url_or_source",
                    "URL or source to check the price at?",
                    None,
                ),
                var(
                    "threshold_pct",
                    "Alert when the price moves by what percent?",
                    Some("5"),
                ),
                var(
                    "target_price",
                    "Alert when the price crosses (blank to skip)?",
                    Some(""),
                ),
            ],
        },
    ]
}

/// Fill `{{name}}` placeholders from `vars`.
///
/// Errors when the prompt references a variable with no value, or when a
/// provided var is not declared by the template (usually a typo). A var
/// with an empty-string value is provided, not missing.
pub fn render_prompt(
    template: &ScheduleTemplate,
    vars: &HashMap<String, String>,
) -> Result<String, String> {
    let known: HashSet<&str> = template.vars.iter().map(|v| v.name.as_str()).collect();
    let mut unknown: Vec<&str> = vars
        .keys()
        .map(String::as_str)
        .filter(|k| !known.contains(k) && !is_reserved_var(k))
        .collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        return Err(format!(
            "template '{}' has no variable(s): {}",
            template.name,
            unknown.join(", ")
        ));
    }
    let mut out = String::with_capacity(template.prompt.len());
    let mut rest = template.prompt.as_str();
    let mut missing: Vec<String> = Vec::new();
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        if !missing.iter().any(|m| m == name) {
                            missing.push(name.to_string());
                        }
                        out.push_str(&format!("{{{{{name}}}}}"));
                    }
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    if !missing.is_empty() {
        return Err(format!(
            "template '{}' missing variable(s): {}",
            template.name,
            missing.join(", ")
        ));
    }
    Ok(out)
}

/// Vars the CLI intercepts instead of substituting into the prompt:
/// `model` (and `provider`) become the job's model pin, not prompt text.
pub fn is_reserved_var(name: &str) -> bool {
    matches!(name, "model" | "provider")
}

/// Insert template defaults for vars the caller did not provide.
pub fn apply_defaults(template: &ScheduleTemplate, vars: &mut HashMap<String, String>) {
    for v in &template.vars {
        if !vars.contains_key(&v.name) {
            if let Some(d) = &v.default {
                vars.insert(v.name.clone(), d.clone());
            }
        }
    }
}

/// The template store: embedded built-ins overlaid with the user's
/// `<data_dir>/templates/*.toml`. A user file with a built-in's name
/// replaces it; anything else is added.
pub struct TemplateStore {
    templates: Vec<ScheduleTemplate>,
}

impl TemplateStore {
    pub fn builtins() -> Self {
        Self {
            templates: builtin_templates(),
        }
    }

    /// Built-ins plus the user dir. Broken user files are skipped —
    /// `schedule create` still validates the final schedule loudly.
    pub fn load(data_dir: &Path) -> Self {
        let mut store = Self::builtins();
        store.overlay_user_dir(&data_dir.join("templates"));
        store
    }

    pub fn get(&self, name: &str) -> Option<&ScheduleTemplate> {
        self.templates.iter().find(|t| t.name == name)
    }

    pub fn list(&self) -> &[ScheduleTemplate] {
        &self.templates
    }

    fn overlay_user_dir(&mut self, dir: &Path) {
        let entries = std::fs::read_dir(dir).map(|r| r.filter_map(Result::ok).collect::<Vec<_>>());
        let mut files: Vec<std::path::PathBuf> = entries
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        files.sort();
        for path in files {
            if let Some(t) = load_user_template(&path) {
                if let Some(slot) = self.templates.iter_mut().find(|t0| t0.name == t.name) {
                    *slot = t;
                } else {
                    self.templates.push(t);
                }
            }
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct UserTemplateFile {
    name: String,
    description: String,
    schedule_every: Option<String>,
    schedule_cron: Option<String>,
    prompt: String,
    #[serde(default)]
    vars: Vec<UserVar>,
}

#[derive(Debug, serde::Deserialize)]
struct UserVar {
    name: String,
    question: String,
    default: Option<String>,
}

fn load_user_template(path: &Path) -> Option<ScheduleTemplate> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: UserTemplateFile = toml::from_str(&text).ok()?;
    if file.name.trim().is_empty() || file.prompt.trim().is_empty() {
        return None;
    }
    let schedule = match (file.schedule_every, file.schedule_cron) {
        (Some(d), None) if !d.trim().is_empty() => TemplateSchedule::Every(d.trim().to_string()),
        (None, Some(c)) if !c.trim().is_empty() => {
            // Reject a broken cron here so `template list` never shows a
            // job that could never fire.
            if crate::CronSchedule::parse(c.trim()).is_err() {
                return None;
            }
            TemplateSchedule::Cron(c.trim().to_string())
        }
        _ => return None,
    };
    if file.vars.iter().any(|v| v.name.trim().is_empty()) {
        return None;
    }
    Some(ScheduleTemplate {
        name: file.name.trim().to_string(),
        description: file.description,
        schedule,
        prompt: file.prompt,
        vars: file
            .vars
            .into_iter()
            .map(|v| TemplateVar {
                name: v.name.trim().to_string(),
                question: v.question,
                default: v.default,
            })
            .collect(),
    })
}
