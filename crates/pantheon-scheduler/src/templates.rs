//! Schedule templates: built-in job blueprints plus a user-managed store.
//!
//! A template is a named, documented starting point for creating a job: a
//! default schedule, a prompt with `{{variable}}` placeholders, and the
//! questions needed to fill them. Built-ins are embedded in
//! [`builtin_templates`]; users add or override templates through the
//! [`TemplateStore`] API, which persists user templates to
//! `<data_dir>/templates.json`:
//!
//! ```json
//! { "templates": [
//!     { "name": "my-watch",
//!       "description": "Watch something I care about.",
//!       "schedule": { "every": "1h" },
//!       "prompt": "Check {{thing}} and report back.",
//!       "vars": [
//!         { "name": "thing", "question": "What should I watch?",
//!           "default": "the build" }
//!       ] }
//! ] }
//! ```
//!
//! A user template with a built-in's name overrides it for [`TemplateStore::get`]
//! and [`TemplateStore::list`]; built-ins themselves cannot be deleted.
//! Templates created through [`TemplateStore::save`] are validated loudly
//! a bad schedule is an error at save time, never a silent no-show at fire
//! time. A job names a template in [`Job::template`](crate::Job::template);
//! [`Job::resolve_task`](crate::Job::resolve_task) re-renders it at fire time.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One fill-in variable in a template prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateVar {
    pub name: String,
    pub question: String,
    pub default: Option<String>,
}

/// A template's default schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateSchedule {
    /// An interval, e.g. `"30m"`.
    Every(String),
    /// A cron expression, e.g. `"0 7 * * *"`.
    Cron(String),
}

/// A schedule blueprint: what to run, when, and what it needs to know.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
                1. GitHub - notifications and activity from the last ~12h on my repos: new issues, \
                PRs needing review, failing CI. Use `gh` (gh api notifications, gh run list).\n\
                2. {{topic}} news - what happened in the last 24h: releases, papers, notable launches \
                and discussions. Search the web; link sources.\n\
                3. Today - what's on the calendar: meetings, deadlines, focus blocks. Use the \
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
                action - draft the reply, defer, or ignore. Skip newsletters, CI bots, and automated \
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
                git locally. Report as: RED - needs action / YELLOW - watch / GREEN - healthy. \
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
                thing to change next week. Honest and specific - cite run ids and numbers, no filler."
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
                if the data exists. Flag anything anomalous - a single run at 10x the median cost, a \
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
                hit - sender, subject, one-line gist, and why it matched. End with suggested actions \
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
                prices (create it if absent). Alert - with old price, new price, and % change - when \
                the price moved ≥ {{threshold_pct}}% since the last check, or when it crosses \
                {{target_price}} in either direction (ignore the target condition when blank). If \
                neither fired, reply in one line with the current price and stop - no noise.".into(),
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

/// Expand a template into a job's creation fields: the shared half of the
/// CLI's `schedule create --template` and the dashboard's
/// `POST /api/schedule/jobs`.
///
/// Reserved vars (`model`, `provider`) become the model pin, not prompt
/// text - an explicit pin wins, a blank var is dropped. Defaults fill the
/// remaining gaps; every var left without a value or a default is resolved
/// through `missing_var(name, question)` - the CLI prompts interactively,
/// the dashboard fails loudly.
///
/// A `missing_var` error does not abort the scan: the remaining vars are
/// still offered to the callback (so a caller can collect *every* missing
/// name for a single loud error), and the first error is returned
/// afterwards - without rendering, since a render with unresolved vars
/// cannot succeed. [`render_prompt`] failures propagate unchanged.
///
/// On success `vars` holds the final var map (reserved vars removed),
/// `task` holds the rendered snapshot when the caller left it empty, and
/// the template's own schedule fills `every`/`cron` when the caller
/// specified none (`has_schedule`).
/// The mutable render targets `expand_template` fills: the var map plus
/// the fields a template may set. Bundled so the function signature stays
/// readable as the template surface grows.
pub struct TemplateParams<'a> {
    pub vars: &'a mut HashMap<String, String>,
    pub model: &'a mut Option<String>,
    pub provider: &'a mut Option<String>,
    pub task: &'a mut String,
    pub every: &'a mut Option<String>,
    pub cron: &'a mut Option<String>,
}

pub fn expand_template(
    template: &ScheduleTemplate,
    params: TemplateParams<'_>,
    has_schedule: bool,
    missing_var: &mut dyn FnMut(&str, &str) -> Result<String, String>,
) -> Result<(), String> {
    let TemplateParams {
        vars,
        model,
        provider,
        task,
        every,
        cron,
    } = params;
    // Reserved vars become the job's model pin, not prompt text. An
    // explicit pin wins over the var; a blank var is dropped silently.
    for reserved in ["model", "provider"] {
        if let Some(v) = vars.remove(reserved) {
            if v.trim().is_empty() {
                continue;
            }
            if reserved == "model" && model.is_none() {
                *model = Some(v);
            } else if reserved == "provider" && provider.is_none() {
                *provider = Some(v);
            }
        }
    }
    apply_defaults(template, vars);
    let mut first_err: Option<String> = None;
    for tv in &template.vars {
        let current = vars.get(&tv.name).map(String::as_str).unwrap_or("");
        // A default (even "") satisfies the var without resolving it.
        if !current.is_empty() || tv.default.is_some() {
            continue;
        }
        match missing_var(&tv.name, &tv.question) {
            Ok(answer) => {
                vars.insert(tv.name.clone(), answer);
            }
            // Keep scanning: the caller may be collecting every missing
            // name for one loud error instead of failing on the first.
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    let rendered = render_prompt(template, vars)?;
    if task.is_empty() {
        *task = rendered;
    }
    if !has_schedule {
        match &template.schedule {
            TemplateSchedule::Every(d) => *every = Some(d.clone()),
            TemplateSchedule::Cron(e) => *cron = Some(e.clone()),
        }
    }
    Ok(())
}

/// The on-disk shape of `<data_dir>/templates.json`: user templates only.
/// Built-ins are embedded in the binary and never written.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TemplateFile {
    #[serde(default)]
    templates: Vec<ScheduleTemplate>,
}

/// The template store: embedded built-ins overlaid with the user's
/// `<data_dir>/templates.json`. A user template with a built-in's name
/// overrides it; anything else is added.
///
/// The store is managed through [`TemplateStore::save`] and
/// [`TemplateStore::delete`], which validate loudly and write through to
/// `templates.json`. Hand-edited files are read leniently: entries that
/// fail validation are skipped with a warning.
pub struct TemplateStore {
    templates: Vec<ScheduleTemplate>,
    user: Vec<ScheduleTemplate>,
    data_dir: PathBuf,
}

impl TemplateStore {
    /// Open the store on `data_dir`. A missing or corrupt `templates.json`
    /// means built-ins only; a corrupt file warns on stderr instead of
    /// crashing.
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join("templates.json");
        let user = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                eprintln!(
                    "scheduler: cannot read {}: {e}; using built-in templates only",
                    path.display()
                );
                Vec::new()
            }
            Ok(text) => match serde_json::from_str::<TemplateFile>(&text) {
                Ok(file) => file
                    .templates
                    .into_iter()
                    .filter(|t| {
                        if let Err(e) = validate_template(t) {
                            eprintln!(
                                "scheduler: ignoring invalid template in {}: {e}",
                                path.display()
                            );
                            false
                        } else {
                            true
                        }
                    })
                    .collect(),
                Err(e) => {
                    eprintln!(
                        "scheduler: ignoring corrupt {}: {e}; using built-in templates only",
                        path.display()
                    );
                    Vec::new()
                }
            },
        };
        Self {
            templates: builtin_templates(),
            user,
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Look up a template by name. A user template wins over a built-in
    /// with the same name.
    pub fn get(&self, name: &str) -> Option<&ScheduleTemplate> {
        self.user
            .iter()
            .find(|t| t.name == name)
            .or_else(|| self.templates.iter().find(|t| t.name == name))
    }

    /// All templates: built-ins first (a user template with a built-in's
    /// name is shown in that built-in's slot), then user-only templates in
    /// save order. Consistent with [`TemplateStore::get`]: every name
    /// resolves to the template shown here.
    pub fn list(&self) -> Vec<&ScheduleTemplate> {
        let mut out: Vec<&ScheduleTemplate> = self
            .templates
            .iter()
            .map(|b| self.user.iter().find(|u| u.name == b.name).unwrap_or(b))
            .collect();
        out.extend(self.user.iter().filter(|u| !self.is_builtin(&u.name)));
        out
    }

    /// Save (add or replace) a user template, validating loudly and
    /// writing through to `templates.json`. A template with a built-in's
    /// name overrides that built-in.
    pub fn save(&mut self, t: ScheduleTemplate) -> Result<(), String> {
        validate_template(&t)?;
        if let Some(slot) = self.user.iter_mut().find(|u| u.name == t.name) {
            *slot = t;
        } else {
            self.user.push(t);
        }
        self.write_through()
    }

    /// Delete a user template. Deleting a user template that overrides a
    /// built-in removes the override and reveals the same-named built-in
    /// again; the built-in itself is never deleted. Deleting a built-in
    /// name with no user override is an error, as is an unknown name.
    pub fn delete(&mut self, name: &str) -> Result<(), String> {
        let before = self.user.len();
        self.user.retain(|u| u.name != name);
        if self.user.len() == before {
            if self.is_builtin(name) {
                return Err(format!(
                    "cannot delete built-in template '{name}'; save a template with the same name to override it"
                ));
            }
            return Err(format!("unknown template '{name}'"));
        }
        self.write_through()
    }

    /// Is this the name of an embedded built-in template?
    pub fn is_builtin(&self, name: &str) -> bool {
        self.templates.iter().any(|t| t.name == name)
    }

    fn write_through(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.data_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.data_dir.display()))?;
        let path = self.data_dir.join("templates.json");
        let text = serde_json::to_string_pretty(&TemplateFile {
            templates: self.user.clone(),
        })
        .map_err(|e| format!("cannot serialize templates: {e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// Loud validation, shared by [`TemplateStore::save`] and the lenient
/// [`TemplateStore::open`] filter.
fn validate_template(t: &ScheduleTemplate) -> Result<(), String> {
    if t.name.trim().is_empty() {
        return Err("template name cannot be empty".to_string());
    }
    if t.prompt.trim().is_empty() {
        return Err(format!("template '{}' has an empty prompt", t.name));
    }
    if t.vars.iter().any(|v| v.name.trim().is_empty()) {
        return Err(format!(
            "template '{}' has a variable with an empty name",
            t.name
        ));
    }
    if let TemplateSchedule::Cron(expr) = &t.schedule {
        crate::CronSchedule::parse(expr)
            .map(|_| ())
            .map_err(|e| format!("template '{}' has an invalid cron schedule: {e}", t.name))?;
    }
    Ok(())
}
