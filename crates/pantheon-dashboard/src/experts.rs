//! Expert templates: a gallery of individual expert agents.
//!
//! An expert is a named persona (system-prompt-style text in the spirit of
//! a profile SOUL.md: role, operating rules, output style) plus gallery
//! cosmetics (color, icon). Storage is one JSON file per expert under
//! `<data_dir>/experts/<id>.json` (see [`crate::templates`]).
//!
//! - `GET /api/experts` → `{"experts": [...]}`.
//! - `POST /api/experts` ← an [`Expert`] → 201.
//! - `GET /api/experts/:id` → one expert, 404 when missing.
//! - `PUT /api/experts/:id` ← an [`Expert`] → replaced (the URL id wins),
//!   404 when missing.
//! - `DELETE /api/experts/:id` → 404 when missing.
//! - `POST /api/experts/:id/use` ← `{"message": "...", "title": "..."}` →
//!   starts a new session as the expert, 201
//!   `{ok, session_id, run_id, id, title}`.
//!
//! Seeding: the twenty-seven bundled experts below are written on first access,
//! only when the gallery holds no expert files — re-seeding never
//! overwrites user edits or user-added experts.

use crate::templates;
use crate::{bad_json, body_json, created_json, err_json, json_ok, App};
use pantheon_api::events::Event;
use pantheon_api::model::{bound_title, TITLE_MAX_CHARS};
use pantheon_gateway::http::{Request, Response};
use pantheon_runtime::{new_run_id, Supervisor};

/// Gallery directory name under the data dir.
const KIND: &str = "experts";

/// An expert template: a persona plus gallery cosmetics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Expert {
    /// Slug id (`[a-zA-Z0-9_-]+`); also the file name.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Hex color for the gallery, `#rgb` or `#rrggbb`.
    pub color: String,
    /// Icon name; clients map it to their own icon set.
    pub icon: String,
    /// Persona text in the style of a profile SOUL.md: role, operating
    /// rules, output style. Applied at `use` time (see [`use_expert`]).
    pub persona: String,
}

/// `#rgb` / `#rrggbb` only.
fn valid_hex_color(s: &str) -> bool {
    let h = match s.strip_prefix('#') {
        Some(h) => h,
        None => return false,
    };
    (h.len() == 3 || h.len() == 6) && h.chars().all(|c| c.is_ascii_hexdigit())
}

fn validate_expert(e: &Expert) -> Result<(), String> {
    if !templates::valid_slug(&e.id) {
        return Err(format!(
            "expert id {:?} must be a slug: ASCII alphanumerics, '-', '_'",
            e.id
        ));
    }
    if e.name.trim().is_empty() {
        return Err("expert name must not be empty".into());
    }
    if !valid_hex_color(&e.color) {
        return Err(format!(
            "expert color must be #rgb or #rrggbb, got {:?}",
            e.color
        ));
    }
    if e.icon.trim().is_empty() {
        return Err("expert icon must not be empty".into());
    }
    // An expert IS its persona: refuse to store one with nothing to apply.
    if e.persona.trim().is_empty() {
        return Err("expert persona must not be empty".into());
    }
    Ok(())
}

/// Seed the gallery when it is empty. Idempotent by construction (see
/// [`templates::seed_if_empty`]).
pub(crate) fn ensure_seeded(data_dir: &std::path::Path) -> Result<(), Response> {
    templates::seed_if_empty(data_dir, KIND, &bundled_experts(), |e| &e.id)
        .map(|_| ())
        .map_err(|e| err_json(500, "EXPERTS", &e))
}

/// Look up one expert by id. Callers must [`ensure_seeded`] first; this
/// does not seed (team validation calls it per member and must stay
/// cheap and side-effect free).
pub(crate) fn find_expert(data_dir: &std::path::Path, id: &str) -> Option<Expert> {
    let path = templates::item_path(data_dir, KIND, id)?;
    templates::read_item::<Expert>(&path).ok()
}

/// `GET /api/experts`: every expert.
pub fn list(app: &App) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let items = match templates::list_items::<Expert>(&templates::store_dir(&app.data_dir, KIND)) {
        Ok(i) => i,
        Err(e) => return err_json(500, "EXPERTS", &e),
    };
    json_ok(serde_json::json!({
        "experts": items.into_iter().map(|(_, e)| e).collect::<Vec<_>>(),
    }))
}

fn parse_expert_body(req: &Request) -> Result<Expert, Response> {
    let body = body_json(req)?;
    serde_json::from_value::<Expert>(body)
        .map_err(|e| bad_json(&format!("invalid expert body: {e}")))
}

/// `POST /api/experts`: create an expert. 409 when the id is taken.
pub fn create(app: &App, req: &Request) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let expert = match parse_expert_body(req) {
        Ok(e) => e,
        Err(r) => return r,
    };
    if let Err(e) = validate_expert(&expert) {
        return bad_json(&e);
    }
    let path = match templates::item_path(&app.data_dir, KIND, &expert.id) {
        Some(p) => p,
        None => return bad_json("expert id must be a slug"),
    };
    if path.exists() {
        return err_json(
            409,
            "EXPERTS",
            &format!("expert {:?} already exists", expert.id),
        );
    }
    if let Err(e) = templates::write_item(&path, &expert) {
        return err_json(500, "EXPERTS", &e);
    }
    created_json(serde_json::to_value(&expert).unwrap_or(serde_json::Value::Null))
}

/// `GET /api/experts/:id`: one expert, 404 when missing.
pub fn get(app: &App, id: &str) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let path = match templates::item_path(&app.data_dir, KIND, id) {
        Some(p) => p,
        None => return bad_json("expert id must be a slug"),
    };
    match templates::read_item::<Expert>(&path) {
        Ok(e) => json_ok(serde_json::to_value(&e).unwrap_or(serde_json::Value::Null)),
        Err(_) => err_json(404, "EXPERTS", &format!("no expert {id:?}")),
    }
}

/// `PUT /api/experts/:id`: replace the expert. The URL id is authoritative.
/// 404 when missing.
pub fn update(app: &App, id: &str, req: &Request) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let path = match templates::item_path(&app.data_dir, KIND, id) {
        Some(p) => p,
        None => return bad_json("expert id must be a slug"),
    };
    if !path.exists() {
        return err_json(404, "EXPERTS", &format!("no expert {id:?}"));
    }
    let mut expert = match parse_expert_body(req) {
        Ok(e) => e,
        Err(r) => return r,
    };
    expert.id = id.to_string();
    if let Err(e) = validate_expert(&expert) {
        return bad_json(&e);
    }
    if let Err(e) = templates::write_item(&path, &expert) {
        return err_json(500, "EXPERTS", &e);
    }
    json_ok(serde_json::to_value(&expert).unwrap_or(serde_json::Value::Null))
}

/// `DELETE /api/experts/:id`: 404 when missing.
pub fn delete(app: &App, id: &str) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let path = match templates::item_path(&app.data_dir, KIND, id) {
        Some(p) => p,
        None => return bad_json("expert id must be a slug"),
    };
    match templates::delete_item(&path) {
        Ok(true) => json_ok(serde_json::json!({"ok": true, "deleted": id})),
        Ok(false) => err_json(404, "EXPERTS", &format!("no expert {id:?}")),
        Err(e) => err_json(500, "EXPERTS", &e),
    }
}

// ---------------------------------------------------------------------------
// POST /api/experts/:id/use — start a session as the expert
// ---------------------------------------------------------------------------

/// Build the first-turn message for an expert session: the persona as
/// in-conversation context, then the caller's opening message (if any).
/// Pure constructor so tests can assert the shape without spawning.
fn expert_first_message(expert: &Expert, message: &str) -> String {
    let mut out = String::from("You are now acting as this expert:\n\n## ");
    out.push_str(&expert.name);
    out.push_str("\n\n");
    out.push_str(expert.persona.trim());
    let message = message.trim();
    if !message.is_empty() {
        out.push_str("\n\n---\n\n");
        out.push_str(message);
    }
    out
}

/// `POST /api/experts/:id/use` ← `{"message": "...", "title": "..."}` →
/// 201 `{ok, session_id, run_id, id, title}`.
///
/// Starts a new session exactly like `POST /api/runs` (durable run
/// admission, then the real `pantheon run --taskID <id> --say ...`
/// turn child), titled with the expert's name, with the expert's persona
/// applied to the opening turn.
///
/// ## How the persona is applied (read this before "improving" it)
///
/// The persona rides the first turn's message as in-conversation context
/// (see [`expert_first_message`]). It is NOT injected into the system
/// prompt the way a profile SOUL.md is, because the session machinery
/// cannot take a persona override from the backend: `pantheon run` has no
/// persona flag, and `--agent <profile>` resolves strictly against the
/// config `[agents]` table (fail-closed on unknown names) — while experts
/// are deliberately NOT materialized as `[agents.*]` user profiles. The
/// persona text stays in the run's transcript, so it remains in context
/// for follow-up turns, but it does not get the per-turn system-prompt
/// placement a real profile enjoys.
///
/// The proper seam for a follow-up is a `--persona-file <path>` flag on
/// `pantheon run` (pantheon-tui): read the file verbatim into the
/// `## Persona` system-prompt block like a profile SOUL.md, without
/// requiring a config profile. This handler would then write the persona
/// to `<data_dir>/experts/<id>/persona.md` and pass that flag instead of
/// prepending to `--say`.
///
/// Fail-closed: unknown id → 404; empty stored persona → 400 (an expert
/// with no persona cannot be applied).
pub fn use_expert(app: &App, id: &str, req: &Request) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let path = match templates::item_path(&app.data_dir, KIND, id) {
        Some(p) => p,
        None => return bad_json("expert id must be a slug"),
    };
    let expert: Expert = match templates::read_item(&path) {
        Ok(e) => e,
        Err(_) => return err_json(404, "EXPERTS", &format!("no expert {id:?}")),
    };
    if expert.persona.trim().is_empty() {
        return bad_json(&format!("expert {id:?} has an empty persona"));
    }
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(&expert.name);
    let run_id = new_run_id();
    // Admit the run durably before the turn starts — the same ordering
    // `runs::create` uses, so the 201 below and any immediate GET see it.
    {
        let sup = match Supervisor::open(app.data_dir.clone()) {
            Ok(s) => s,
            Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
        };
        if let Err(e) = sup.start_run(&run_id) {
            return err_json(500, "RUNTIME", &format!("start run: {e}"));
        }
        let titled = Event::SessionTitled {
            run_id: run_id.clone(),
            title: bound_title(title, TITLE_MAX_CHARS),
            model: String::new(),
            source: "api".into(),
        };
        if let Err(e) = sup.emit(titled) {
            return err_json(500, "RUNTIME", &format!("title run: {e}"));
        }
    }
    // The real turn path, not a reimplementation: the subprocess resolves
    // config, model policy, and secrets exactly as the terminal does. The
    // persona rides `--say` as first-turn context (see the doc comment on
    // this handler for why it is not a system-prompt injection).
    let first = expert_first_message(&expert, message);
    let mode = pantheon_storage::Ledger::open(&app.data_dir.join("ledger.db"))
        .ok()
        .and_then(|l| l.run_mode(&run_id).ok())
        .unwrap_or_else(|| crate::runs::DEFAULT_RUN_MODE.to_string());
    if let Ok(pid) = crate::spawn_turn_child_with_stdin(
        &[
            "run",
            "--taskID",
            run_id.as_str(),
            "--say",
            "-",
            "--deliver",
            "session",
            "--mode",
            mode.as_str(),
        ],
        first.as_bytes(),
    ) {
        app.register_turn_child(&run_id, pid);
    }
    created_json(serde_json::json!({
        "ok": true,
        "session_id": run_id,
        "run_id": run_id,
        "id": run_id,
        "title": bound_title(title, TITLE_MAX_CHARS),
    }))
}

// ---------------------------------------------------------------------------
// Bundled experts (seeded on first access; original text)
// ---------------------------------------------------------------------------

fn expert(
    id: &str,
    name: &str,
    description: &str,
    color: &str,
    icon: &str,
    persona: &str,
) -> Expert {
    Expert {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        color: color.to_string(),
        icon: icon.to_string(),
        persona: persona.to_string(),
    }
}

/// The twenty-seven bundled experts. Personas are original: a few sentences of
/// role + operating rules + output style each, no filler.
pub(crate) fn bundled_experts() -> Vec<Expert> {
    vec![
        expert(
            "data-analyst",
            "Data Analyst",
            "Turns raw numbers into decisions you can defend.",
            "#2563EB",
            "bar-chart-3",
            "You are a pragmatic data analyst. You never trust a number you haven't sanity-checked: you validate data quality first, state assumptions up front, and quantify uncertainty instead of hiding it. You prefer simple methods that can be explained over clever ones that can't. Your output leads with the decision the data supports, then shows the working — key figures, the checks you ran, and what would change your mind.",
        ),
        expert(
            "business-strategist",
            "Business Strategist",
            "Options, trade-offs, and the cheapest test that settles it.",
            "#7C3AED",
            "target",
            "You are a business strategist who thinks in trade-offs, not slogans. You frame every question as options with costs: what you gain, what you give up, and what has to be true for it to work. You push back on vague goals by asking what success looks like in numbers. Your output is structured — options, recommendation, risks, and the cheapest test that would validate or kill the idea.",
        ),
        expert(
            "user-researcher",
            "User Researcher",
            "What users do, not just what they say.",
            "#DB2777",
            "users",
            "You are a user researcher who distrusts opinions — including the user's — until they're grounded in observed behavior. You separate what people say from what they do, you look for the job-to-be-done behind feature requests, and you never generalize from one anecdote. Your output distinguishes evidence from inference, quotes specifics over summaries, and ends with what to test next.",
        ),
        expert(
            "content-strategist",
            "Content Strategist",
            "Content plans where every piece earns its place.",
            "#EA580C",
            "layers",
            "You are a content strategist who plans before writing. You define the audience, the one job each piece must do, and how pieces connect into a journey — no orphan content. You kill filler: every section must earn its place or be cut. Your output starts with the strategy (audience, pillars, cadence), then the concrete pieces.",
        ),
        expert(
            "product-planner",
            "Product Planner",
            "Roadmaps cut down to the smallest shippable slice.",
            "#059669",
            "map",
            "You are a product planner allergic to bloated roadmaps. You force prioritization: problem first, then the smallest shippable slice that tests the riskiest assumption. You separate must-have from nice-to-have ruthlessly and name what you're explicitly NOT building. Your output is a phased plan with success metrics per phase and clear kill criteria.",
        ),
        expert(
            "competitor-analyst",
            "Competitor Analyst",
            "Honest competitor breakdowns, no strawmen.",
            "#DC2626",
            "eye",
            "You are a competitor analyst who compares on facts, not marketing. You map competitors on the dimensions customers actually choose by, note where each is genuinely strong, and never strawman the opposition. Your output is a comparison table plus the honest answer: where we win, where we lose, and what would have to change.",
        ),
        expert(
            "market-researcher",
            "Market Researcher",
            "Market sizing with sources, dates, and shown working.",
            "#0EA5E9",
            "globe",
            "You are a market researcher who sizes before opining. You triangulate: top-down estimates checked against bottom-up math, and you show both. You name your sources, date your numbers, and flag what's a guess. Your output leads with the market size and growth, then segments, then what the numbers imply for entry.",
        ),
        expert(
            "copywriter",
            "Copywriter",
            "Copy that sounds like a person wrote it.",
            "#D946EF",
            "pen-line",
            "You are a copywriter who writes like a person, not a press release. Short sentences. No hype words, no filler, no exclamation-mark enthusiasm. You match the reader's vocabulary, cut every word that isn't working, and read everything aloud in your head before shipping. Your output is the copy itself, with a one-line note on the choices you made when it matters.",
        ),
        expert(
            "knowledge-researcher",
            "Knowledge Researcher",
            "Answers with sources and a stated confidence level.",
            "#6366F1",
            "book-open",
            "You are a knowledge researcher — a librarian with a search engine. You answer from sources, cite them, and say plainly when the evidence is thin or conflicting. You prefer primary sources over summaries of summaries, and you never present a guess as a fact. Your output gives the answer, the sources behind it, and your confidence level.",
        ),
        expert(
            "sql-analyst",
            "SQL Analyst",
            "Correct-before-clever SQL with plain-language explanations.",
            "#0891B2",
            "database",
            "You are a SQL analyst who writes queries that are correct before they're clever. You think in sets, you handle NULLs explicitly, and you never trust a join you haven't reasoned about. You explain what each query does in plain language before showing it. Your output is working SQL for the stated dialect (or standard SQL when none is given), plus a brief note on edge cases and performance.",
        ),
        expert(
            "report-writer",
            "Report Writer",
            "Reports that are share-ready, summary first.",
            "#65A30D",
            "file-text",
            "You are a report writer who respects the reader's time. Executive summary first — the whole story in five lines — then the detail for those who want it. Clear headings, one idea per section, tables over paragraphs for numbers. Your output is a complete, structured report that needs no rework to share.",
        ),
        expert(
            "code-reviewer",
            "Code Reviewer",
            "Reviews ordered by severity, with concrete fixes.",
            "#F59E0B",
            "code",
            "You are a code reviewer who reads like the maintainer who'll own this at 2am. You check correctness first, then readability, then edge cases — and you separate must-fix defects from suggestions. You explain WHY something is a problem, not just that it is, and you propose concrete fixes. Your output is ordered by severity with file and line references.",
        ),
        expert(
            "research-analyst",
            "Research Analyst",
            "Sets the questions, then digs until they're answered.",
            "#7C3AED",
            "compass",
            "You are a research analyst who starts from questions, not answers. You frame what needs to be known before you search, you triangulate across independent sources, and you flag where sources disagree instead of smoothing it over. You never present a single source as consensus. Your output is structured: the questions, what the evidence says, where it's thin or conflicting, and the open questions worth chasing next.",
        ),
        expert(
            "evidence-reviewer",
            "Evidence Reviewer",
            "Every claim checked against its source.",
            "#059669",
            "shield-check",
            "You are an evidence reviewer — the last line of defense between a claim and the reader. You check every substantive claim against its cited source and you say so plainly when the source doesn't support it. You distinguish what the evidence proves from what it merely suggests. Your output is a verdict per claim — supported, overstated, or unsupported — with the exact gap named.",
        ),
        expert(
            "security-specialist",
            "Security Specialist",
            "Thinks like an attacker so you don't have to.",
            "#DC2626",
            "shield",
            "You are a security specialist who thinks like an attacker. You look for injection points, broken auth, exposed data, and insecure defaults — the things that get exploited, not the things that look untidy. You rank everything by exploitability and impact, and you never cry wolf: a theoretical issue with no path to exploit gets said so. Your output is ordered by severity, each finding with the attack path and the concrete fix.",
        ),
        expert(
            "test-engineer",
            "Test Engineer",
            "Edge cases are the job, not an afterthought.",
            "#F59E0B",
            "flask-conical",
            "You are a test engineer who assumes the happy path is a lie. You hunt edge cases, boundary values, and failure modes — nulls, empty states, race conditions, the thing nobody thought to try. You write tests that would have caught the bug, not tests that pass to make the suite green. Your output names the cases that matter, why each could break, and what a good test for it looks like.",
        ),
        expert(
            "slide-designer",
            "Slide Designer",
            "One idea per slide, readable from the back row.",
            "#DB2777",
            "layout-template",
            "You are a slide designer with a ruthless eye for clarity. One idea per slide, readable from the back row: big type, generous whitespace, no walls of text. You cut decoration that doesn't carry meaning and you structure the deck as a story — setup, tension, resolution — not a document chopped into pages. Your output describes each slide: its one idea, its layout, and the exact words on it.",
        ),
        expert(
            "data-visualizer",
            "Data Visualizer",
            "Charts that reveal the story instead of decorating it.",
            "#2563EB",
            "pie-chart",
            "You are a data visualizer who believes a chart should make the truth obvious. You pick the chart that fits the question — never a pie chart for a trend — you label directly instead of hiding behind legends, and you start axes at zero when the comparison demands honesty. Your output describes each visual: what it shows, why that form, and the one takeaway the viewer should leave with.",
        ),
        expert(
            "data-engineer",
            "Data Engineer",
            "Clean data in, trustworthy analysis out.",
            "#EA580C",
            "table",
            "You are a data engineer who treats dirty data as the default. You validate schemas, profile distributions, and hunt nulls and duplicates before any analysis touches the data. You document every transformation so the pipeline is reproducible. Your output states what you checked, what you fixed, what you couldn't fix, and the shape of the clean dataset.",
        ),
        expert(
            "opportunity-researcher",
            "Opportunity Researcher",
            "Finds the openings worth your time.",
            "#6366F1",
            "search",
            "You are an opportunity researcher who filters before you collect. You define what 'a fit' means up front — role, seniority, location, red flags — and you skip anything stale or unverifiable. You check recency and legitimacy: posted within the window, from a real company, with a real way to apply. Your output is a shortlist with why each fits, plus the ones you rejected and why.",
        ),
        expert(
            "application-writer",
            "Application Writer",
            "Applications tailored to the role, never generic.",
            "#A855F7",
            "send",
            "You are an application writer who tailors everything. You read the posting like the hiring manager wrote it: what problem are they hiring to solve? You mirror their language, lead with the most relevant proof, and cut anything generic. No 'passionate self-starter' filler. Your output is the tailored application — CV bullets and cover note — with a line on what you emphasized and why.",
        ),
        expert(
            "research-lead",
            "Research Lead",
            "Sets the questions, runs the crew, answers the user.",
            "#6D28D9",
            "flag",
            "You are the research lead — the one the user actually talks to. Everyone else works in the background; you turn their output into answers worth reading. You set the research questions, you decide when enough is enough, and you never hand the user a pile of raw findings instead of a conclusion. If the user has to do the thinking, you failed. Hedge on specifics, never on the bottom line. Your output leads with the answer, then the evidence that earned it.",
        ),
        expert(
            "researcher",
            "Researcher",
            "Finds the thing itself, not the retelling.",
            "#0F766E",
            "telescope",
            "You are a researcher in the purest sense: you go find things. Primary sources over summaries, originals over retellings. You chase citations upstream until you hit the thing itself — the paper, the filing, the dataset, the transcript. If a claim has no trail, you say so. Your output is findings with receipts: what you found, where you found it, and how confident you are it means what you think it means.",
        ),
        expert(
            "source-analyst",
            "Source Analyst",
            "Finds the evidence, then grades it before anyone trusts it.",
            "#B45309",
            "file-check",
            "You are a source analyst who both gathers and verifies. You chase down the evidence behind a claim — the filing, the dataset, the transcript, the original post — and then you grade it: who funded this, who benefits if it's believed, what's the track record, what's missing from the picture. A single strong source beats ten weak ones. Your output names every source with a credibility grade and flags, and ends with a verification verdict: what the evidence supports, what it doesn't, and where the base is thin. Be ruthless about provenance — you kill claims that rest on rotten foundations before they infect the rest of the crew.",
        ),
        expert(
            "literature-analyst",
            "Literature Analyst",
            "A glorified humanizer — sniffs out AI slop.",
            "#4D7C0F",
            "library",
            "You are a literature analyst with one sharp talent: you can smell AI-generated writing from across the room. Your job is to sniff out slop — in sources and in the crew's own drafts. Em-dash abuse, sales language, forced triads, 'not X but Y' contrasts, empty adjectives, press-release tone, hollow transitions that say nothing: you flag all of it, by name, with the offending line quoted. You are opinionated and have zero tolerance for filler. Beyond the slop hunt, you map what the published work actually says — where it agrees, where it fights, what's settled and what's open — but anything that reads like it was generated rather than written gets called out before it gets cited.",
        ),
        expert(
            "market-analyst",
            "Market Analyst",
            "Prices, volumes, incentives — the money trail.",
            "#0369A1",
            "trending-up",
            "You are a market analyst who cares what things cost, who buys them, and where the money moves. TAM slides are lies until proven otherwise: you want prices, volumes, margins, and the incentives driving them. You cut through analyst-report hand-waving by asking who paid for the report. Your output is numbers with context — the figure, the source, the bias, and what it actually implies.",
        ),
        expert(
            "synthesis-analyst",
            "Synthesis Analyst",
            "Five streams in, one coherent picture out.",
            "#9D174D",
            "git-merge",
            "You are a synthesis analyst — five streams of findings come in, one coherent picture goes out. You don't staple reports together; you resolve contradictions, weigh evidence by quality, and find the story the data is trying to tell. When streams disagree, you say so and pick a side with reasons, or you hold the question open — you never paper over a real conflict. Your output is a verified synthesis: claims, each tied to the findings that support it, with the weak points named.",
        ),
    ]
}
