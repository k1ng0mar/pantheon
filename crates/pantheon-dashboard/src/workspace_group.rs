//! workspace_group group: experts, memory, skills, teams, templates, ideas, profiles in one file.

pub mod experts {
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
    //! only when the gallery holds no expert files - re-seeding never
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
        let items =
            match templates::list_items::<Expert>(&templates::store_dir(&app.data_dir, KIND)) {
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
    // POST /api/experts/:id/use - start a session as the expert
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
    /// config `[agents]` table (fail-closed on unknown names) - while experts
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
        // Admit the run durably before the turn starts - the same ordering
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
        match crate::spawn_turn_child_with_stdin(
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
            Ok(pid) => app.register_turn_child(&run_id, pid),
            Err(e) => {
                return err_json(500, "SPAWN", &format!("failed to start turn: {e}"));
            }
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
                "You are a pragmatic data analyst. You never trust a number you haven't sanity-checked: you validate data quality first, state assumptions up front, and quantify uncertainty instead of hiding it. You prefer simple methods that can be explained over clever ones that can't. Your output leads with the decision the data supports, then shows the working - key figures, the checks you ran, and what would change your mind.",
            ),
            expert(
                "business-strategist",
                "Business Strategist",
                "Options, trade-offs, and the cheapest test that settles it.",
                "#7C3AED",
                "target",
                "You are a business strategist who thinks in trade-offs, not slogans. You frame every question as options with costs: what you gain, what you give up, and what has to be true for it to work. You push back on vague goals by asking what success looks like in numbers. Your output is structured - options, recommendation, risks, and the cheapest test that would validate or kill the idea.",
            ),
            expert(
                "user-researcher",
                "User Researcher",
                "What users do, not just what they say.",
                "#DB2777",
                "users",
                "You are a user researcher who distrusts opinions - including the user's - until they're grounded in observed behavior. You separate what people say from what they do, you look for the job-to-be-done behind feature requests, and you never generalize from one anecdote. Your output distinguishes evidence from inference, quotes specifics over summaries, and ends with what to test next.",
            ),
            expert(
                "content-strategist",
                "Content Strategist",
                "Content plans where every piece earns its place.",
                "#EA580C",
                "layers",
                "You are a content strategist who plans before writing. You define the audience, the one job each piece must do, and how pieces connect into a journey - no orphan content. You kill filler: every section must earn its place or be cut. Your output starts with the strategy (audience, pillars, cadence), then the concrete pieces.",
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
                "You are a knowledge researcher - a librarian with a search engine. You answer from sources, cite them, and say plainly when the evidence is thin or conflicting. You prefer primary sources over summaries of summaries, and you never present a guess as a fact. Your output gives the answer, the sources behind it, and your confidence level.",
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
                "You are a report writer who respects the reader's time. Executive summary first - the whole story in five lines - then the detail for those who want it. Clear headings, one idea per section, tables over paragraphs for numbers. Your output is a complete, structured report that needs no rework to share.",
            ),
            expert(
                "code-reviewer",
                "Code Reviewer",
                "Reviews ordered by severity, with concrete fixes.",
                "#F59E0B",
                "code",
                "You are a code reviewer who reads like the maintainer who'll own this at 2am. You check correctness first, then readability, then edge cases - and you separate must-fix defects from suggestions. You explain WHY something is a problem, not just that it is, and you propose concrete fixes. Your output is ordered by severity with file and line references.",
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
                "You are an evidence reviewer - the last line of defense between a claim and the reader. You check every substantive claim against its cited source and you say so plainly when the source doesn't support it. You distinguish what the evidence proves from what it merely suggests. Your output is a verdict per claim - supported, overstated, or unsupported - with the exact gap named.",
            ),
            expert(
                "security-specialist",
                "Security Specialist",
                "Thinks like an attacker so you don't have to.",
                "#DC2626",
                "shield",
                "You are a security specialist who thinks like an attacker. You look for injection points, broken auth, exposed data, and insecure defaults - the things that get exploited, not the things that look untidy. You rank everything by exploitability and impact, and you never cry wolf: a theoretical issue with no path to exploit gets said so. Your output is ordered by severity, each finding with the attack path and the concrete fix.",
            ),
            expert(
                "test-engineer",
                "Test Engineer",
                "Edge cases are the job, not an afterthought.",
                "#F59E0B",
                "flask-conical",
                "You are a test engineer who assumes the happy path is a lie. You hunt edge cases, boundary values, and failure modes - nulls, empty states, race conditions, the thing nobody thought to try. You write tests that would have caught the bug, not tests that pass to make the suite green. Your output names the cases that matter, why each could break, and what a good test for it looks like.",
            ),
            expert(
                "slide-designer",
                "Slide Designer",
                "One idea per slide, readable from the back row.",
                "#DB2777",
                "layout-template",
                "You are a slide designer with a ruthless eye for clarity. One idea per slide, readable from the back row: big type, generous whitespace, no walls of text. You cut decoration that doesn't carry meaning and you structure the deck as a story - setup, tension, resolution - not a document chopped into pages. Your output describes each slide: its one idea, its layout, and the exact words on it.",
            ),
            expert(
                "data-visualizer",
                "Data Visualizer",
                "Charts that reveal the story instead of decorating it.",
                "#2563EB",
                "pie-chart",
                "You are a data visualizer who believes a chart should make the truth obvious. You pick the chart that fits the question - never a pie chart for a trend - you label directly instead of hiding behind legends, and you start axes at zero when the comparison demands honesty. Your output describes each visual: what it shows, why that form, and the one takeaway the viewer should leave with.",
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
                "You are an opportunity researcher who filters before you collect. You define what 'a fit' means up front - role, seniority, location, red flags - and you skip anything stale or unverifiable. You check recency and legitimacy: posted within the window, from a real company, with a real way to apply. Your output is a shortlist with why each fits, plus the ones you rejected and why.",
            ),
            expert(
                "application-writer",
                "Application Writer",
                "Applications tailored to the role, never generic.",
                "#A855F7",
                "send",
                "You are an application writer who tailors everything. You read the posting like the hiring manager wrote it: what problem are they hiring to solve? You mirror their language, lead with the most relevant proof, and cut anything generic. No 'passionate self-starter' filler. Your output is the tailored application - CV bullets and cover note - with a line on what you emphasized and why.",
            ),
            expert(
                "research-lead",
                "Research Lead",
                "Sets the questions, runs the crew, answers the user.",
                "#6D28D9",
                "flag",
                "You are the research lead - the one the user actually talks to. Everyone else works in the background; you turn their output into answers worth reading. You set the research questions, you decide when enough is enough, and you never hand the user a pile of raw findings instead of a conclusion. If the user has to do the thinking, you failed. Hedge on specifics, never on the bottom line. Your output leads with the answer, then the evidence that earned it.",
            ),
            expert(
                "researcher",
                "Researcher",
                "Finds the thing itself, not the retelling.",
                "#0F766E",
                "telescope",
                "You are a researcher in the purest sense: you go find things. Primary sources over summaries, originals over retellings. You chase citations upstream until you hit the thing itself - the paper, the filing, the dataset, the transcript. If a claim has no trail, you say so. Your output is findings with receipts: what you found, where you found it, and how confident you are it means what you think it means.",
            ),
            expert(
                "source-analyst",
                "Source Analyst",
                "Finds the evidence, then grades it before anyone trusts it.",
                "#B45309",
                "file-check",
                "You are a source analyst who both gathers and verifies. You chase down the evidence behind a claim - the filing, the dataset, the transcript, the original post - and then you grade it: who funded this, who benefits if it's believed, what's the track record, what's missing from the picture. A single strong source beats ten weak ones. Your output names every source with a credibility grade and flags, and ends with a verification verdict: what the evidence supports, what it doesn't, and where the base is thin. Be ruthless about provenance - you kill claims that rest on rotten foundations before they infect the rest of the crew.",
            ),
            expert(
                "literature-analyst",
                "Literature Analyst",
                "A glorified humanizer - sniffs out AI slop.",
                "#4D7C0F",
                "library",
                "You are a literature analyst with one sharp talent: you can smell AI-generated writing from across the room. Your job is to sniff out slop - in sources and in the crew's own drafts. Em-dash abuse, sales language, forced triads, 'not X but Y' contrasts, empty adjectives, press-release tone, hollow transitions that say nothing: you flag all of it, by name, with the offending line quoted. You are opinionated and have zero tolerance for filler. Beyond the slop hunt, you map what the published work actually says - where it agrees, where it fights, what's settled and what's open - but anything that reads like it was generated rather than written gets called out before it gets cited.",
            ),
            expert(
                "market-analyst",
                "Market Analyst",
                "Prices, volumes, incentives - the money trail.",
                "#0369A1",
                "trending-up",
                "You are a market analyst who cares what things cost, who buys them, and where the money moves. TAM slides are lies until proven otherwise: you want prices, volumes, margins, and the incentives driving them. You cut through analyst-report hand-waving by asking who paid for the report. Your output is numbers with context - the figure, the source, the bias, and what it actually implies.",
            ),
            expert(
                "synthesis-analyst",
                "Synthesis Analyst",
                "Five streams in, one coherent picture out.",
                "#9D174D",
                "git-merge",
                "You are a synthesis analyst - five streams of findings come in, one coherent picture goes out. You don't staple reports together; you resolve contradictions, weigh evidence by quality, and find the story the data is trying to tell. When streams disagree, you say so and pick a side with reasons, or you hold the question open - you never paper over a real conflict. Your output is a verified synthesis: claims, each tied to the findings that support it, with the weak points named.",
            ),
        ]
    }
}

pub mod memory {
    //! Memory browser: agent-layer records with provenance.
    //!
    //! Reads through the real [`pantheon_memory`] backend selection
    //! (`open_selected`), so the dashboard sees whatever backend the install
    //! uses. The consolidation score is honestly `null`: the persisted record
    //! carries no score field, and the dashboard will not invent one.

    use crate::util::{now_ms, DEFAULT_AGENT_NAME};
    use crate::{bad_json, body_json, created_json, err_json, json_ok, query_usize, App};
    use pantheon_api::capability::Policy;
    use pantheon_api::error::PantheonError;
    use pantheon_api::provenance::TrustTier;
    use pantheon_gateway::http::{Request, Response};
    use pantheon_memory::{
        open_selected, write_via, LayerKind, MemoryBackend, Proposal, Provenance,
    };

    /// Cap on the serialized proposal bytes for a single `write_via` memory
    /// write; the policy gate rejects larger proposals.
    const MAX_MEMORY_WRITE_BYTES: usize = 4096;

    fn namespace_of(req: &Request) -> String {
        req.query
            .get("namespace")
            .cloned()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("PANTHEON_MEMORY_NAMESPACE").ok())
            .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string())
    }

    /// `GET /api/memory?namespace=&q=&limit=`
    pub fn browse(app: &App, req: &Request) -> Response {
        let ns = namespace_of(req);
        let limit = query_usize(&req.query, "limit", 200).min(1000);
        let q = req.query.get("q").map(|s| s.to_lowercase());
        let backend = match open_selected(&app.data_dir) {
            Ok(b) => b,
            Err(e) => return err_json(500, "MEMORY", &format!("open backend: {e}")),
        };
        let pairs = match backend.list_agent(&ns) {
            Ok(p) => p,
            Err(e) => return err_json(500, "MEMORY", &format!("list: {e}")),
        };
        let mut out = Vec::new();
        for (key, value) in pairs {
            if let Some(qq) = &q {
                if !key.to_lowercase().contains(qq) && !value.to_lowercase().contains(qq) {
                    continue;
                }
            }
            // Provenance comes from the full record; a backend without `get`
            // still yields the key/value row.
            let (provenance, recorded_at_ms) = match backend.get(LayerKind::Agent, &ns, &key) {
                Ok(Some(rec)) => (
                    serde_json::json!({
                        "source": rec.provenance.source,
                        "origin": rec.provenance.origin,
                        "trust": format!("{:?}", rec.provenance.trust).to_lowercase(),
                    }),
                    Some(rec.provenance.recorded_at_ms),
                ),
                _ => (serde_json::Value::Null, None),
            };
            out.push(serde_json::json!({
                "key": key,
                "value": value,
                "layer": "agent",
                "namespace": ns,
                "provenance": provenance,
                "recorded_at_ms": recorded_at_ms,
                // No persisted score exists on the record; null, honestly.
                "score": serde_json::Value::Null,
            }));
            if out.len() >= limit {
                break;
            }
        }
        json_ok(serde_json::json!({"namespace": ns, "records": out}))
    }

    /// Derive a stable, human-readable key from the text: the first words
    /// slugified, plus a short hash so two similar notes never collide into
    /// an upsert. `kind`, when given, namespaces the key.
    fn derive_key(kind: Option<&str>, text: &str) -> String {
        let mut slug: String = text
            .split_whitespace()
            .take(8)
            .filter_map(|w| {
                let clean: String = w
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_lowercase();
                if clean.is_empty() {
                    None
                } else {
                    Some(clean)
                }
            })
            .collect::<Vec<_>>()
            .join("-");
        if slug.is_empty() {
            slug = "note".to_string();
        }
        // ASCII-only by construction, so byte truncation is char-safe.
        if slug.len() > 48 {
            slug.truncate(48);
        }
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        now_ms().hash(&mut h);
        let key = format!("{slug}-{:06x}", h.finish() % 0xffffff);
        match kind {
            Some(k) => format!("{k}:{key}"),
            None => key,
        }
    }

    /// `POST /api/memory`: store a memory entry (the write path `browse` lacks).
    ///
    /// Body: `{"text": "...", "kind": "..."}` (`kind` optional). Mirrors
    /// `pantheon memory put`: agent layer, the request namespace (query
    /// `?namespace=`, env, or the default), user-trust provenance stamped by
    /// the API. The key is derived from the text - the CLI's explicit key has
    /// no equivalent in this body shape. Returns 201 with the entry in browse
    /// shape.
    pub fn add(app: &App, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let text = body
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if text.is_empty() {
            return bad_json("field \"text\" is required");
        }
        let kind = body
            .get("kind")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|k| !k.is_empty());
        let ns = namespace_of(req);
        let backend = match open_selected(&app.data_dir) {
            Ok(b) => b,
            Err(e) => return err_json(500, "MEMORY", &format!("open backend: {e}")),
        };
        let record = match write_via(
            backend.as_ref(),
            &Policy::coder_with_memory(),
            Proposal {
                layer: LayerKind::Agent,
                namespace: ns.clone(),
                key: derive_key(kind, text),
                value: text.to_string(),
                provenance: Provenance {
                    source: "api".into(),
                    origin: "user".into(),
                    trust: TrustTier::User,
                    recorded_at_ms: now_ms(),
                },
            },
            MAX_MEMORY_WRITE_BYTES,
        ) {
            Ok(r) => r,
            Err(e) => return err_json(500, "MEMORY", &format!("write: {e}")),
        };
        created_json(serde_json::json!({
            "key": record.key,
            "value": record.value,
            "layer": "agent",
            "namespace": record.namespace,
            "provenance": {
                "source": record.provenance.source,
                "origin": record.provenance.origin,
                "trust": format!("{:?}", record.provenance.trust).to_lowercase(),
            },
            "recorded_at_ms": record.provenance.recorded_at_ms,
            "score": serde_json::Value::Null,
        }))
    }

    /// Does `id` name an entry in the namespace? Checked through
    /// `list_agent` - the same read path `browse` uses - because external
    /// backends are query-oriented and may not implement `get`.
    fn entry_exists(
        backend: &dyn MemoryBackend,
        ns: &str,
        id: &str,
    ) -> Result<bool, PantheonError> {
        Ok(backend.list_agent(ns)?.iter().any(|(k, _)| k == id))
    }

    fn entry_or_404(backend: &dyn MemoryBackend, ns: &str, id: &str) -> Option<Response> {
        match entry_exists(backend, ns, id) {
            Ok(true) => None,
            Ok(false) => Some(err_json(
                404,
                "NOT_FOUND",
                &format!("no memory entry '{id}' in namespace '{ns}'"),
            )),
            Err(e) => Some(err_json(500, "MEMORY", &format!("list: {e}"))),
        }
    }

    /// `DELETE /api/memory/:id`: remove one agent-layer entry from the
    /// request namespace (`?namespace=`, env, or the default). 404 when the
    /// id names no entry.
    pub fn remove(app: &App, req: &Request, id: &str) -> Response {
        let ns = namespace_of(req);
        let backend = match open_selected(&app.data_dir) {
            Ok(b) => b,
            Err(e) => return err_json(500, "MEMORY", &format!("open backend: {e}")),
        };
        if let Some(r) = entry_or_404(backend.as_ref(), &ns, id) {
            return r;
        }
        match backend.forget(LayerKind::Agent, &ns, id) {
            Ok(_) => json_ok(serde_json::json!({"ok": true, "key": id, "namespace": ns})),
            Err(e) => err_json(500, "MEMORY", &format!("forget: {e}")),
        }
    }

    /// `PUT /api/memory/:id`: correct/replace an entry's text in place (same
    /// key, same layer and namespace). Body: `{"text": "..."}` - 400 on
    /// empty text, 404 on unknown id. The replacement is stamped as a user
    /// edit via the API (the same provenance shape `add` uses); the native
    /// store upserts on (layer, namespace, key), and the User trust tier
    /// wins the upsert guard, so a correction always lands.
    pub fn replace(app: &App, req: &Request, id: &str) -> Response {
        let body = match body_json(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let text = body
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if text.is_empty() {
            return bad_json("field \"text\" is required");
        }
        let ns = namespace_of(req);
        let backend = match open_selected(&app.data_dir) {
            Ok(b) => b,
            Err(e) => return err_json(500, "MEMORY", &format!("open backend: {e}")),
        };
        if let Some(r) = entry_or_404(backend.as_ref(), &ns, id) {
            return r;
        }
        let record = match write_via(
            backend.as_ref(),
            &Policy::coder_with_memory(),
            Proposal {
                layer: LayerKind::Agent,
                namespace: ns.clone(),
                key: id.to_string(),
                value: text.to_string(),
                provenance: Provenance {
                    source: "api".into(),
                    origin: "user".into(),
                    trust: TrustTier::User,
                    recorded_at_ms: now_ms(),
                },
            },
            MAX_MEMORY_WRITE_BYTES,
        ) {
            Ok(r) => r,
            Err(e) => return err_json(500, "MEMORY", &format!("write: {e}")),
        };
        json_ok(serde_json::json!({
            "key": record.key,
            "value": record.value,
            "layer": "agent",
            "namespace": record.namespace,
            "provenance": {
                "source": record.provenance.source,
                "origin": record.provenance.origin,
                "trust": format!("{:?}", record.provenance.trust).to_lowercase(),
            },
            "recorded_at_ms": record.provenance.recorded_at_ms,
            "score": serde_json::Value::Null,
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::swarm;
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicU64, Ordering};

        static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

        fn test_app() -> (App, std::path::PathBuf) {
            let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "pantheon-dashboard-memory-test-{}-{}",
                std::process::id(),
                n
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp data dir");
            let app = App {
                data_dir: dir.clone(),
                token: "test-token".to_string(),
                bind: "127.0.0.1".to_string(),
                bind_all: false,
                on_approval: None,
                send_locks: Default::default(),
                ui_action_locks: Default::default(),
                turn_children: Default::default(),
                swarm: swarm::orchestrator_for(&dir),
                desktop: Default::default(),
            };
            (app, dir)
        }

        fn json_req(method: &str, body: &str) -> Request {
            let mut query = HashMap::new();
            query.insert("namespace".to_string(), "test-ns".to_string());
            Request {
                method: method.to_string(),
                path: "/api/memory".to_string(),
                query,
                headers: HashMap::new(),
                body: body.as_bytes().to_vec(),
            }
        }

        fn status_body(resp: Response) -> (u16, serde_json::Value) {
            match resp {
                Response::Buffered { status, body, .. } => (
                    status,
                    serde_json::from_slice(&body).expect("response body is JSON"),
                ),
                _ => panic!("expected a buffered response"),
            }
        }

        /// Add one entry through the real handler; returns its derived key.
        fn add_entry(app: &App, text: &str) -> String {
            let (status, body) = status_body(add(
                app,
                &json_req("POST", &format!(r#"{{"text":{text:?}}}"#, text = text)),
            ));
            assert_eq!(status, 201, "{body}");
            body["key"].as_str().expect("key in response").to_string()
        }

        fn listed_keys(app: &App) -> Vec<String> {
            let (status, body) = status_body(browse(app, &json_req("GET", "")));
            assert_eq!(status, 200, "{body}");
            body["records"]
                .as_array()
                .expect("records array")
                .iter()
                .map(|r| r["key"].as_str().expect("key").to_string())
                .collect()
        }

        #[test]
        fn replace_updates_entry_text() {
            let (app, dir) = test_app();
            let key = add_entry(&app, "the sky is blue");
            let (status, body) = status_body(replace(
                &app,
                &json_req("PUT", r#"{"text":"the sky is actually azure"}"#),
                &key,
            ));
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["key"].as_str(), Some(key.as_str()));
            assert_eq!(body["value"].as_str(), Some("the sky is actually azure"));
            // Same key, new text visible through browse.
            assert!(listed_keys(&app).contains(&key));
            let (status, body) = status_body(browse(&app, &json_req("GET", "")));
            assert_eq!(status, 200);
            let rec = body["records"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["key"].as_str() == Some(key.as_str()))
                .expect("entry still listed");
            assert_eq!(rec["value"].as_str(), Some("the sky is actually azure"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn replace_rejects_unknown_id_and_empty_text() {
            let (app, dir) = test_app();
            let (status, body) = status_body(replace(
                &app,
                &json_req("PUT", r#"{"text":"whatever"}"#),
                "no-such-key",
            ));
            assert_eq!(status, 404, "{body}");
            assert_eq!(body["error"]["code"].as_str(), Some("NOT_FOUND"));
            let key = add_entry(&app, "keep me");
            let (status, body) =
                status_body(replace(&app, &json_req("PUT", r#"{"text":"   "}"#), &key));
            assert_eq!(status, 400, "{body}");
            // The failed correction left the original text alone.
            assert!(listed_keys(&app).contains(&key));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn remove_deletes_entry_and_404s_afterwards() {
            let (app, dir) = test_app();
            let key = add_entry(&app, "temporary note");
            assert!(listed_keys(&app).contains(&key));
            let (status, body) = status_body(remove(&app, &json_req("DELETE", ""), &key));
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["ok"].as_bool(), Some(true));
            assert!(!listed_keys(&app).contains(&key));
            // Second delete is a 404, not a silent no-op.
            let (status, body) = status_body(remove(&app, &json_req("DELETE", ""), &key));
            assert_eq!(status, 404, "{body}");
            assert_eq!(body["error"]["code"].as_str(), Some("NOT_FOUND"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn remove_404s_on_unknown_id() {
            let (app, dir) = test_app();
            let (status, body) = status_body(remove(&app, &json_req("DELETE", ""), "no-such-key"));
            assert_eq!(status, 404, "{body}");
            assert_eq!(body["error"]["code"].as_str(), Some("NOT_FOUND"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod skills {
    //! Skills: list, enable/disable, import, delete.
    //!
    //! Discovery is the real `pantheon_exec::skills` scan (bundled seeding
    //! included). The enabled/disabled flag is the shared
    //! `<data_dir>/skills/disabled.json` registry - the same file the
    //! session loader reads, so a dashboard toggle genuinely removes the
    //! skill from the model's toolset. Delete is limited to pantheon-scope
    //! skills (under `<data_dir>/skills`); foreign scopes (hermes, claude,
    //! ...) can be disabled but never deleted by the dashboard.

    use crate::{bad_json, body_json, err_json, json_ok, App};
    use pantheon_exec::skills::{
        disabled_skill_names, discover_skills_ext, import_skill_from_url, import_skills_from_repo,
        set_skill_disabled,
    };
    use pantheon_gateway::http::{Request, Response};

    use crate::util::project_root;

    /// `GET /api/skills?scope=pantheon|external`
    pub fn list(app: &App, req: &Request) -> Response {
        let disabled = disabled_skill_names(&app.data_dir);
        let pantheon_root = app.data_dir.join("skills");
        let scope = req.query.get("scope").map(String::as_str).unwrap_or("");
        let skills = discover_skills_ext(&app.data_dir, &project_root(), &[]);
        let out: Vec<serde_json::Value> = skills
            .iter()
            .filter(|s| {
                let pantheon_scope = s.path.starts_with(&pantheon_root);
                match scope {
                    "pantheon" => pantheon_scope,
                    "external" => !pantheon_scope,
                    _ => true,
                }
            })
            .map(|s| {
                let enabled = !disabled.iter().any(|d| d == &s.meta.name);
                let pantheon_scope = s.path.starts_with(&pantheon_root);
                serde_json::json!({
                    "name": s.meta.name,
                    "description": s.meta.description,
                    "origin": s.meta.origin,
                    "path": s.path.display().to_string(),
                    "enabled": enabled,
                    "scope": if pantheon_scope { "pantheon" } else { "external" },
                })
            })
            .collect();
        json_ok(serde_json::json!({"skills": out}))
    }

    /// `POST /api/skills/:name/toggle` - flips the shared disabled flag.
    pub fn toggle(app: &App, name: &str) -> Response {
        let enabled = !disabled_skill_names(&app.data_dir)
            .iter()
            .any(|d| d == name);
        set_enabled(app, name, !enabled)
    }

    /// `POST /api/skills/:name/enable` or `.../disable`.
    pub fn set_enabled(app: &App, name: &str, enabled: bool) -> Response {
        // A typo used to write a stray `disabled.json` entry for a skill
        // that does not exist, silently shadowing nothing today and
        // everything if a skill of that name ever appears. Check
        // existence against the same discovery the list endpoint uses.
        let known = discover_skills_ext(&app.data_dir, &project_root(), &[])
            .iter()
            .any(|s| s.meta.name == name);
        if !known {
            return err_json(404, "NOT_FOUND", &format!("no skill named '{name}'"));
        }
        if let Err(e) = set_skill_disabled(&app.data_dir, name, !enabled) {
            return err_json(500, "SKILLS", &e.to_string());
        }
        json_ok(serde_json::json!({"ok": true, "name": name, "enabled": enabled}))
    }

    /// `DELETE /api/skills/:name` - pantheon-scope only.
    pub fn delete(app: &App, name: &str) -> Response {
        if name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || name.trim().is_empty()
        {
            return bad_json("unsafe skill name");
        }
        let dir = app.data_dir.join("skills").join(name);
        let root = app.data_dir.join("skills");
        if !dir.starts_with(&root) || !dir.join("SKILL.md").exists() {
            return err_json(404, "NOT_FOUND", "no pantheon-scope skill by that name");
        }
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            return err_json(500, "SKILLS", &format!("delete: {e}"));
        }
        // Also clear any disabled flag so a re-import starts enabled.
        let _ = set_skill_disabled(&app.data_dir, name, false);
        json_ok(serde_json::json!({"ok": true}))
    }

    /// `POST /api/skills/import`:
    /// `{url, confirm: true}` or `{repo, subpath?, confirm: true}`.
    /// The UI confirms before sending; the endpoint still requires
    /// `confirm: true` before touching the filesystem.
    pub fn import(app: &App, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        if !body
            .get("confirm")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return bad_json("import requires confirm: true");
        }
        if let Some(url) = body.get("url").and_then(|v| v.as_str()) {
            if url.trim().is_empty() {
                return bad_json("url is empty");
            }
            return match import_skill_from_url(&app.data_dir, url) {
                Ok((dest, skill)) => json_ok(serde_json::json!({
                    "ok": true,
                    "imported": [{"name": skill.meta.name, "path": dest.display().to_string()}],
                })),
                Err(e) => err_json(502, "SKILL_IMPORT", &e.to_string()),
            };
        }
        if let Some(repo) = body.get("repo").and_then(|v| v.as_str()) {
            if repo.trim().is_empty() {
                return bad_json("repo is empty");
            }
            let subpath = body
                .get("subpath")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            return match import_skills_from_repo(&app.data_dir, repo, subpath) {
                Ok(list) => json_ok(serde_json::json!({
                    "ok": true,
                    "imported": list.iter().map(|(n, p)| serde_json::json!({
                        "name": n, "path": p.display().to_string(),
                    })).collect::<Vec<_>>(),
                })),
                Err(e) => err_json(502, "SKILL_IMPORT", &e.to_string()),
            };
        }
        bad_json("supply {url} or {repo}")
    }
}

pub mod teams {
    //! Team templates: named multi-agent rosters a user can launch as a swarm.
    //!
    //! A team is genuinely composed of experts: every member carries an
    //! `expert_id` referencing the Experts gallery, and the member's display
    //! identity (name, avatar color, icon) is derived from the linked expert
    //! at read time - the expert is the single source of truth. Members keep
    //! a team-specific `role` line (their job within that team) and the agent
    //! profile they run as. Storage is one JSON file per team under
    //! `<data_dir>/teams/<id>.json` (see [`crate::templates`]).
    //!
    //! - `GET /api/teams` → `{"teams": [...]}` (members enriched with their
    //!   expert's display identity, plus `member_count`).
    //! - `POST /api/teams` ← a [`Team`] → 201 (dangling `expert_id` → 400).
    //! - `GET /api/teams/:id` → one team, 404 when missing.
    //! - `PUT /api/teams/:id` ← a [`Team`] → replaced (the URL id wins), 404
    //!   when missing, dangling `expert_id` → 400.
    //! - `DELETE /api/teams/:id` → 404 when missing.
    //! - `POST /api/teams/:id/use` ← `{"task": "...", "judge": true}` → spawns
    //!   a swarm through the existing orchestrator exactly like
    //!   `swarm::create` profiles mode, and answers like it:
    //!   `{ok, swarm_id, id, run_id, agents, status_view}`. `run_id` is the
    //!   first agent's run id - a swarm is a fan-out, not a single session,
    //!   so session-style screens load that run. `task` overrides the team's
    //!   `brief_template` when non-empty; `judge` defaults to true.
    //!   A member whose `expert_id` no longer resolves (expert deleted after
    //!   the team was written) fails closed: 400, nothing spawns.
    //!
    //! Seeding: the five bundled teams below are written on first access,
    //! only when the gallery holds no team files - re-seeding never
    //! overwrites user edits or user-added teams. The experts gallery is
    //! seeded first, and every bundled member is verified against the bundled
    //! expert list; a broken reference is a code bug, so seeding fails loudly
    //! (500) instead of writing a team that can never launch.

    use crate::templates;
    use crate::{bad_json, body_json, created_json, err_json, json_ok, App};
    use pantheon_gateway::http::{Request, Response};
    use pantheon_runtime::swarm_exec::{
        AgentSpec, ExecutionPlan, PlanTopology, StageSpec, SwarmError,
    };

    /// Gallery directory name under the data dir.
    const KIND: &str = "teams";

    /// A team member: a reference to an expert in the Experts gallery plus
    /// the member's job within this team. Display identity (name, color,
    /// icon) is never stored here - it is derived from the linked expert at
    /// read time (see [`Team::to_client_json`]), so a team is genuinely
    /// composed of experts rather than carrying a stale copy of them.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct TeamMember {
        /// Expert id (`experts/<id>`); must resolve at write time and at
        /// `use` time (fail closed on dangling references).
        pub expert_id: String,
        /// One-line job description within this team (roster UI).
        pub role: String,
        /// Agent profile ref (`[agents.<name>]`); `"default"` is the
        /// conventional default profile (see [`resolve_member_profile`]).
        pub profile: String,
    }

    /// How a team executes, as a primary label. A team may combine patterns
    /// (Deep Research is fan-out → merge → review loop); the `stages` carry
    /// the real flow, `topology` names the dominant one for the gallery.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Topology {
        /// Stages run in order, each feeding the next.
        Pipeline,
        /// Members work in parallel, then results merge.
        ParallelMerge,
        /// Stages run in order with a bounded verify loop.
        ReviewLoop,
    }

    /// One step of a team's execution flow.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct Stage {
        /// Stage name, unique within the team; `loop_back_to` targets it.
        pub name: String,
        /// Expert ids working this stage; must be a subset of the team's
        /// members. Several ids here means parallel work within the stage.
        pub members: Vec<String>,
        /// What this stage consumes, as a structured contract (e.g.
        /// `"claims[] as {statement, source_url}"`) - data, not prose dumps.
        pub input_contract: String,
        /// What this stage produces, as a structured contract (e.g.
        /// `"verdict {pass: bool, failed_claims[]}"`).
        pub output_contract: String,
        /// Optional review edge: on verification failure, redo this earlier
        /// stage. Must name an *earlier* stage - forward references are
        /// unexecutable, so they fail validation (the stage graph stays a
        /// DAG plus bounded backward review edges).
        pub loop_back_to: Option<String>,
    }

    /// A launchable team template.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct Team {
        /// Slug id (`[a-zA-Z0-9_-]+`); also the file name.
        pub id: String,
        pub name: String,
        pub description: String,
        /// Task brief used when `POST /api/teams/:id/use` gets no `task`
        /// override. Supports a `{{task}}` placeholder for callers that
        /// render it; otherwise used as-is.
        pub brief_template: String,
        pub members: Vec<TeamMember>,
        /// Primary execution pattern; `stages` carry the real flow.
        pub topology: Topology,
        /// The lead's expert id: the ONLY member that talks to the user.
        /// Must be one of `members`. The lead's run is the user-facing one
        /// at `use` time.
        pub lead_expert_id: String,
        /// Ordered execution flow; each stage hands structured data (not
        /// prose) to the next via its contracts.
        pub stages: Vec<Stage>,
    }

    impl Team {
        fn member_count(&self) -> usize {
            self.members.len()
        }

        fn to_json(&self) -> serde_json::Value {
            let mut v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
            v["member_count"] = serde_json::json!(self.member_count());
            v
        }

        /// Client-facing JSON: the stored shape plus `member_count`, with
        /// every member enriched by its expert's display identity:
        /// `{expert_id, role, profile, expert: {id, name, color, icon}}`.
        /// `expert` is `null` when the linked expert no longer exists (e.g.
        /// deleted after the team was written) - clients render those members
        /// as unresolved, and `use` refuses to launch them (fail closed).
        /// `lead` carries the lead expert's display identity (null when the
        /// lead's expert is gone); stages keep expert ids, which clients join
        /// against `members`.
        fn to_client_json(&self, data_dir: &std::path::Path) -> serde_json::Value {
            let mut v = self.to_json();
            let expert_json = |id: &str| {
                crate::experts::find_expert(data_dir, id).map(|e| {
                    serde_json::json!({
                        "id": e.id,
                        "name": e.name,
                        "color": e.color,
                        "icon": e.icon,
                    })
                })
            };
            let members: Vec<serde_json::Value> = self
                .members
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "expert_id": m.expert_id,
                        "role": m.role,
                        "profile": m.profile,
                        "expert": expert_json(&m.expert_id),
                    })
                })
                .collect();
            v["members"] = serde_json::Value::Array(members);
            v["lead"] = expert_json(&self.lead_expert_id).unwrap_or(serde_json::Value::Null);
            v
        }
    }

    /// A member is only valid if its expert exists: a team genuinely
    /// composed of experts cannot reference one that isn't there.
    /// Structural validation: no data-dir access, so it is safe to re-run
    /// at `use` time on a hand-edited team file. Covers the topology model:
    /// the lead must be a member, stage members must be a subset of the
    /// team, and review edges must point at earlier stages (the stage graph
    /// stays a DAG plus bounded backward review edges - forward references
    /// would be unexecutable).
    fn validate_structure(t: &Team) -> Result<(), String> {
        if !templates::valid_slug(&t.id) {
            return Err(format!(
                "team id {:?} must be a slug: ASCII alphanumerics, '-', '_'",
                t.id
            ));
        }
        if t.name.trim().is_empty() {
            return Err("team name must not be empty".into());
        }
        if t.members.is_empty() || t.members.len() > 8 {
            return Err(format!(
                "team must have 1..=8 members (swarm cap), got {}",
                t.members.len()
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for m in &t.members {
            if m.expert_id.trim().is_empty() {
                return Err("member expert_id must not be empty".into());
            }
            if !templates::valid_slug(&m.expert_id) {
                return Err(format!(
                    "member expert_id {:?} must be a slug: ASCII alphanumerics, '-', '_'",
                    m.expert_id
                ));
            }
            if !seen.insert(m.expert_id.as_str()) {
                return Err(format!("duplicate member {:?}", m.expert_id));
            }
            if m.role.trim().is_empty() {
                return Err(format!("member {:?} role must not be empty", m.expert_id));
            }
            if m.profile.trim().is_empty() {
                return Err(format!(
                    "member {:?} profile must not be empty",
                    m.expert_id
                ));
            }
        }
        // The lead is the only member that talks to the user: it must be one
        // of the members.
        if !t.members.iter().any(|m| m.expert_id == t.lead_expert_id) {
            return Err(format!(
                "lead expert {:?} is not a team member",
                t.lead_expert_id
            ));
        }
        if t.stages.is_empty() {
            return Err("team must define at least one stage".into());
        }
        let mut stage_names = std::collections::HashSet::new();
        for (i, s) in t.stages.iter().enumerate() {
            if s.name.trim().is_empty() {
                return Err(format!("stage {} name must not be empty", i + 1));
            }
            if !stage_names.insert(s.name.as_str()) {
                return Err(format!("duplicate stage name {:?}", s.name));
            }
            if s.members.is_empty() {
                return Err(format!("stage {:?} must have at least one member", s.name));
            }
            for em in &s.members {
                if !t.members.iter().any(|m| &m.expert_id == em) {
                    return Err(format!(
                        "stage {:?} references {:?}, which is not a team member",
                        s.name, em
                    ));
                }
            }
            if s.input_contract.trim().is_empty() {
                return Err(format!(
                    "stage {:?} input_contract must not be empty",
                    s.name
                ));
            }
            if s.output_contract.trim().is_empty() {
                return Err(format!(
                    "stage {:?} output_contract must not be empty",
                    s.name
                ));
            }
            if let Some(target) = &s.loop_back_to {
                match t.stages[..i].iter().position(|p| &p.name == target) {
                    Some(_) => {}
                    None => {
                        if t.stages.iter().any(|p| &p.name == target) {
                            return Err(format!(
                                "stage {:?} loops back to {:?}, which is not an earlier stage (forward references are unexecutable)",
                                s.name, target
                            ));
                        }
                        return Err(format!(
                            "stage {:?} loops back to unknown stage {:?}",
                            s.name, target
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Full write-time validation: structure plus expert existence. A team
    /// is genuinely composed of experts, so a member referencing one that
    /// isn't in the gallery is rejected.
    fn validate_team(data_dir: &std::path::Path, t: &Team) -> Result<(), String> {
        validate_structure(t)?;
        for m in &t.members {
            if crate::experts::find_expert(data_dir, &m.expert_id).is_none() {
                return Err(format!(
                    "unknown expert {:?}: no such expert in the Experts gallery",
                    m.expert_id
                ));
            }
        }
        Ok(())
    }

    /// Seed the gallery when it is empty. Idempotent by construction (see
    /// [`templates::seed_if_empty`]).
    ///
    /// The experts gallery seeds first: teams reference experts, so every
    /// bundled member is verified against the bundled expert list before
    /// anything is written. A dangling bundled reference is a code bug
    /// fail loudly (500) rather than seeding a team that can never launch.
    /// (An expert a user deletes later is a runtime condition, handled
    /// per-team at `use` time - it must not break the gallery.)
    fn ensure_seeded(data_dir: &std::path::Path) -> Result<(), Response> {
        crate::experts::ensure_seeded(data_dir)?;
        templates::seed_if_empty(data_dir, KIND, &bundled_teams(), |t| &t.id)
            .map_err(|e| err_json(500, "TEAMS", &e))?;
        let bundled = crate::experts::bundled_experts();
        let known: std::collections::HashSet<&str> =
            bundled.iter().map(|e| e.id.as_str()).collect();
        for team in bundled_teams() {
            for m in &team.members {
                if !known.contains(m.expert_id.as_str()) {
                    return Err(err_json(
                        500,
                        "TEAMS",
                        &format!(
                            "bundled team {:?} references unknown expert {:?}",
                            team.id, m.expert_id
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn team_response(data_dir: &std::path::Path, team: &Team) -> Response {
        json_ok(team.to_client_json(data_dir))
    }

    /// `GET /api/teams`: every team, members enriched with expert identity,
    /// plus `member_count`.
    pub fn list(app: &App) -> Response {
        if let Err(r) = ensure_seeded(&app.data_dir) {
            return r;
        }
        let items = match templates::list_items::<Team>(&templates::store_dir(&app.data_dir, KIND))
        {
            Ok(i) => i,
            Err(e) => return err_json(500, "TEAMS", &e),
        };
        json_ok(serde_json::json!({
            "teams": items.into_iter().map(|(_, t)| t.to_client_json(&app.data_dir)).collect::<Vec<_>>(),
        }))
    }

    fn parse_team_body(req: &Request) -> Result<Team, Response> {
        let body = body_json(req)?;
        serde_json::from_value::<Team>(body)
            .map_err(|e| bad_json(&format!("invalid team body: {e}")))
    }

    /// `POST /api/teams`: create a team. 409 when the id is taken.
    pub fn create(app: &App, req: &Request) -> Response {
        if let Err(r) = ensure_seeded(&app.data_dir) {
            return r;
        }
        let team = match parse_team_body(req) {
            Ok(t) => t,
            Err(r) => return r,
        };
        if let Err(e) = validate_team(&app.data_dir, &team) {
            return bad_json(&e);
        }
        let path = match templates::item_path(&app.data_dir, KIND, &team.id) {
            Some(p) => p,
            None => return bad_json("team id must be a slug"),
        };
        if path.exists() {
            return err_json(409, "TEAMS", &format!("team {:?} already exists", team.id));
        }
        if let Err(e) = templates::write_item(&path, &team) {
            return err_json(500, "TEAMS", &e);
        }
        created_json(team.to_client_json(&app.data_dir))
    }

    /// `GET /api/teams/:id`: one team, 404 when missing.
    pub fn get(app: &App, id: &str) -> Response {
        if let Err(r) = ensure_seeded(&app.data_dir) {
            return r;
        }
        let path = match templates::item_path(&app.data_dir, KIND, id) {
            Some(p) => p,
            None => return bad_json("team id must be a slug"),
        };
        match templates::read_item::<Team>(&path) {
            Ok(t) => team_response(&app.data_dir, &t),
            Err(_) => err_json(404, "TEAMS", &format!("no team {id:?}")),
        }
    }

    /// `PUT /api/teams/:id`: replace the team. The URL id is authoritative
    /// a body id that disagrees is overwritten. 404 when missing.
    pub fn update(app: &App, id: &str, req: &Request) -> Response {
        if let Err(r) = ensure_seeded(&app.data_dir) {
            return r;
        }
        let path = match templates::item_path(&app.data_dir, KIND, id) {
            Some(p) => p,
            None => return bad_json("team id must be a slug"),
        };
        if !path.exists() {
            return err_json(404, "TEAMS", &format!("no team {id:?}"));
        }
        let mut team = match parse_team_body(req) {
            Ok(t) => t,
            Err(r) => return r,
        };
        team.id = id.to_string();
        if let Err(e) = validate_team(&app.data_dir, &team) {
            return bad_json(&e);
        }
        if let Err(e) = templates::write_item(&path, &team) {
            return err_json(500, "TEAMS", &e);
        }
        team_response(&app.data_dir, &team)
    }

    /// `DELETE /api/teams/:id`: 404 when missing.
    pub fn delete(app: &App, id: &str) -> Response {
        if let Err(r) = ensure_seeded(&app.data_dir) {
            return r;
        }
        let path = match templates::item_path(&app.data_dir, KIND, id) {
            Some(p) => p,
            None => return bad_json("team id must be a slug"),
        };
        match templates::delete_item(&path) {
            Ok(true) => json_ok(serde_json::json!({"ok": true, "deleted": id})),
            Ok(false) => err_json(404, "TEAMS", &format!("no team {id:?}")),
            Err(e) => err_json(500, "TEAMS", &e),
        }
    }

    // ---------------------------------------------------------------------------
    // POST /api/teams/:id/use - profile resolution and swarm launch
    // ---------------------------------------------------------------------------

    /// Resolve a member's profile ref to the concrete profile the swarm
    /// agent should run as.
    ///
    /// `"default"` is the conventional default profile name
    /// (`pantheon_api::agent_profile::DEFAULT_PROFILE` - "the identity used
    /// when no profile is selected"). It resolves exactly like
    /// `Config::resolve_profile` does: the config's active `agent` (the same
    /// field `profiles.rs` reads as the default profile) wins, else the
    /// literal `"default"`. The resolved name must be declared in `[agents]`,
    /// except in the fully-anonymous case (no active agent and an empty or
    /// missing `[agents]` table), where `"default"` passes through
    /// untouched: swarm count mode already runs such turns without `--agent`
    /// (see `swarm::agent_flag_for`), so the bundled teams stay usable on
    /// installs that predate profiles.
    ///
    /// Any other name must be declared in `[agents]`, exactly like
    /// `swarm::create` profiles mode - unknown → Err naming the bad profile,
    /// and nothing spawns (fail closed; a typo must never silently run the
    /// wrong identity).
    fn resolve_member_profile(
        cfg: Option<&pantheon_api::config::Config>,
        profile: &str,
    ) -> Result<String, String> {
        let profile = profile.trim();
        if profile.is_empty() {
            return Err("member profile must not be empty".into());
        }
        let default_name = pantheon_api::agent_profile::DEFAULT_PROFILE;
        if profile != default_name {
            let known = cfg
                .as_ref()
                .map(|c| c.agents.contains_key(profile))
                .unwrap_or(false);
            return if known {
                Ok(profile.to_string())
            } else {
                Err(format!(
                    "unknown profile \"{profile}\": not declared in [agents]"
                ))
            };
        }
        // "default": the active `agent` wins (profiles.rs's default-profile
        // field); it must be declared - an active name pointing nowhere is a
        // broken config, fail closed on it.
        let active = cfg
            .as_ref()
            .and_then(|c| c.agent.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(name) = active {
            let known = cfg
                .as_ref()
                .map(|c| c.agents.contains_key(name))
                .unwrap_or(false);
            return if known {
                Ok(name.to_string())
            } else {
                Err(format!(
                    "unknown profile \"{name}\": the active `agent` is not declared in [agents]"
                ))
            };
        }
        if cfg
            .as_ref()
            .map(|c| c.agents.contains_key(default_name))
            .unwrap_or(false)
        {
            return Ok(default_name.to_string());
        }
        // Anonymous install: nothing declared at all. Pass "default" through;
        // the turn child degrades to no `--agent` instead of failing.
        Ok(default_name.to_string())
    }

    fn swarm_err(e: SwarmError) -> Response {
        err_json(e.http_status, &e.code, &e.message)
    }

    /// `POST /api/teams/:id/use` ← `{"task": "...", "judge": true}`.
    ///
    /// Fail-closed validation happens before anything spawns: structural
    /// validation first (lead/stages survive hand-edits), then every member
    /// must resolve to a real expert, then every profile must resolve. Any
    /// failure is a 400 and nothing spawns.
    ///
    /// Render the lead task from the team's brief template and the caller's
    /// `task` override. `{{task}}` is a placeholder, not a task: an
    /// override renders into the template when it has the placeholder, a
    /// template containing the placeholder without an override is a
    /// missing-task error (the old code launched a run whose transcript
    /// literally started `Review brief: {{task}}`), and a template without
    /// the placeholder keeps the old behavior (override wins, else the
    /// template itself is the task).
    fn render_brief(template: &str, task_override: &str) -> Result<String, &'static str> {
        if template.contains("{{task}}") {
            if task_override.is_empty() {
                return Err(
                    "field \"task\" is required: this team's brief_template contains {{task}}",
                );
            }
            Ok(template.replace("{{task}}", task_override))
        } else if task_override.is_empty() {
            if template.is_empty() {
                Err("field \"task\" is required (or set the team's brief_template)")
            } else {
                Ok(template.to_string())
            }
        } else {
            Ok(task_override.to_string())
        }
    }

    /// The team's stages become an [`ExecutionPlan`] and execution goes
    /// through the swarm orchestrator's staged path (`create_staged`), which
    /// mechanically runs the topology: stages in order, handoff inputs
    /// between them, bounded review loops, member runs never user-facing.
    /// The response mirrors `swarm::create`'s shape, plus `run_id` - the
    /// lead's coordination run, the only user-facing run.
    pub fn use_team(app: &App, id: &str, req: &Request) -> Response {
        if let Err(r) = ensure_seeded(&app.data_dir) {
            return r;
        }
        let path = match templates::item_path(&app.data_dir, KIND, id) {
            Some(p) => p,
            None => return bad_json("team id must be a slug"),
        };
        let team: Team = match templates::read_item(&path) {
            Ok(t) => t,
            Err(_) => return err_json(404, "TEAMS", &format!("no team {id:?}")),
        };
        let body = match body_json(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let task_override = body
            .get("task")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        let base_task = match render_brief(team.brief_template.trim(), task_override) {
            Ok(t) => t,
            Err(e) => return bad_json(e),
        };
        let judge = body.get("judge").and_then(|v| v.as_bool()).unwrap_or(true);
        if team.members.is_empty() || team.members.len() > 8 {
            return bad_json("team must have 1..=8 members to launch");
        }
        if let Err(e) = validate_structure(&team) {
            return bad_json(&e);
        }
        let cfg = crate::session_factory::load_config(&app.data_dir)
            .ok()
            .flatten();
        let mut resolved: Vec<crate::experts::Expert> = Vec::with_capacity(team.members.len());
        for m in &team.members {
            match crate::experts::find_expert(&app.data_dir, &m.expert_id) {
                Some(e) => resolved.push(e),
                None => {
                    return bad_json(&format!(
                        "team {:?} references unknown expert {:?}: the expert was deleted or never existed; nothing was launched",
                        team.id, m.expert_id
                    ))
                }
            };
        }
        let expert_by_id: std::collections::HashMap<&str, &crate::experts::Expert> =
            resolved.iter().map(|e| (e.id.as_str(), e)).collect();
        // Resolve every member's profile before building the plan.
        let mut profile_by_id: std::collections::HashMap<&str, String> =
            std::collections::HashMap::with_capacity(team.members.len());
        for (m, e) in team.members.iter().zip(resolved.iter()) {
            match resolve_member_profile(cfg.as_ref(), &m.profile) {
                Ok(p) => {
                    profile_by_id.insert(e.id.as_str(), p);
                }
                Err(e) => return bad_json(&e),
            }
        }
        let lead = match expert_by_id.get(team.lead_expert_id.as_str()) {
            Some(e) => *e,
            None => return bad_json("team lead does not resolve to an expert"),
        };
        let lead_profile = profile_by_id
            .get(team.lead_expert_id.as_str())
            .cloned()
            .unwrap_or_default();
        let topology = match team.topology {
            Topology::Pipeline => PlanTopology::Pipeline,
            Topology::ParallelMerge => PlanTopology::ParallelMerge,
            Topology::ReviewLoop => PlanTopology::ReviewLoop,
        };
        let mut stages = Vec::with_capacity(team.stages.len());
        for s in &team.stages {
            let mut members = Vec::with_capacity(s.members.len());
            for member_id in &s.members {
                match expert_by_id.get(member_id.as_str()) {
                    Some(e) => members.push(AgentSpec {
                        name: e.name.clone(),
                        profile: profile_by_id
                            .get(member_id.as_str())
                            .cloned()
                            .unwrap_or_default(),
                    }),
                    None => {
                        return bad_json(&format!(
                            "stage {:?} references unknown member expert {:?}",
                            s.name, member_id
                        ))
                    }
                }
            }
            let loop_back_to = match &s.loop_back_to {
                Some(name) => match team.stages.iter().position(|o| &o.name == name) {
                    Some(i) => Some(i),
                    None => {
                        return bad_json(&format!(
                            "stage {:?} loops back to unknown stage {name:?}",
                            s.name
                        ))
                    }
                },
                None => None,
            };
            stages.push(StageSpec {
                name: s.name.clone(),
                members,
                input_contract: s.input_contract.clone(),
                output_contract: s.output_contract.clone(),
                loop_back_to,
            });
        }
        let plan = ExecutionPlan {
            team_name: team.name.clone(),
            topology,
            stages,
            lead_name: lead.name.clone(),
            lead_profile,
            max_review_iterations: 3,
        };
        match app.swarm.create_staged(&base_task, plan, judge) {
            Ok(created) => {
                // The lead's coordination run is the user-facing run.
                let run_id = created.status.lead_run_id.clone().unwrap_or_default();
                let agent_names: Vec<&str> = created
                    .status
                    .agents
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect();
                let mut out = serde_json::json!({
                    "ok": true,
                    "swarm_id": created.id,
                    "id": created.id,
                    "run_id": run_id,
                    "agents": agent_names,
                });
                out["status_view"] =
                    serde_json::to_value(&created.status).unwrap_or(serde_json::Value::Null);
                created_json(out)
            }
            Err(e) => swarm_err(e),
        }
    }

    // ---------------------------------------------------------------------------
    // Bundled teams (seeded on first access)
    // ---------------------------------------------------------------------------

    fn member(expert_id: &str, role: &str) -> TeamMember {
        TeamMember {
            expert_id: expert_id.to_string(),
            role: role.to_string(),
            // Bundled members run as the conventional default profile; the
            // concrete identity resolves at `use` time (see
            // `resolve_member_profile`). Never validated at seed time - the
            // user's config is unknown until then.
            profile: pantheon_api::agent_profile::DEFAULT_PROFILE.to_string(),
        }
    }

    /// Bundled-team stage builder. `loop_back_to` names an earlier stage
    /// (backward review edge); `None` for plain forward stages.
    fn stage(
        name: &str,
        members: &[&str],
        input_contract: &str,
        output_contract: &str,
        loop_back_to: Option<&str>,
    ) -> Stage {
        Stage {
            name: name.to_string(),
            members: members.iter().map(|s| s.to_string()).collect(),
            input_contract: input_contract.to_string(),
            output_contract: output_contract.to_string(),
            loop_back_to: loop_back_to.map(|s| s.to_string()),
        }
    }

    /// The five bundled teams. Every member references a real expert id;
    /// rosters are Umar's spec - teams are genuinely composed of experts.
    fn bundled_teams() -> Vec<Team> {
        vec![
            Team {
                id: "deep-research".into(),
                name: "Deep Research".into(),
                description: "Fan-out research crew: four analysts gather, one synthesizes, one writes.".into(),
                brief_template: "Research brief: {{task}}\n\nInvestigate the topic above thoroughly. Four analysts gather findings in parallel, a synthesis analyst merges them into one verified picture, and a report writer turns it into the final report. Every substantive claim must cite a source. Flag disagreements between sources explicitly, and end with a short list of open questions the evidence could not settle.".into(),
                members: vec![
                    member("research-lead", "Sets the research questions, runs the crew, answers the user"),
                    member("researcher", "Finds primary sources and original material"),
                    member("source-analyst", "Gathers evidence and verifies every source before anyone trusts it"),
                    member("literature-analyst", "Sniffs out AI-generated slop and hollow writing in sources and drafts"),
                    member("market-analyst", "Supplies prices, volumes, and incentives"),
                    member("synthesis-analyst", "Merges the four streams into one verified synthesis"),
                    member("report-writer", "Turns the synthesis into the final report"),
                ],
                topology: Topology::ParallelMerge,
                lead_expert_id: "research-lead".into(),
                stages: vec![
                    stage(
                        "gather",
                        &[
                            "researcher",
                            "source-analyst",
                            "literature-analyst",
                            "market-analyst",
                        ],
                        "research questions + scope",
                        "contributions[] as {member, findings[] as {finding, evidence, source_url, confidence}}; source-analyst's contribution additionally carries {sources[]: {url, credibility, flags[]}, verification_verdict}",
                        None,
                    ),
                    stage(
                        "synthesize",
                        &["synthesis-analyst"],
                        "contributions[] (four streams)",
                        "synthesis with claims[] as {statement, supporting_findings[], confidence}",
                        None,
                    ),
                    stage(
                        "report",
                        &["report-writer"],
                        "synthesis with claims[]",
                        "final report (markdown)",
                        None,
                    ),
                ],
            },
            Team {
                id: "pitch-deck".into(),
                name: "Pitch Deck".into(),
                description: "Turns a raw idea into a tight, persuasive deck.".into(),
                brief_template: "Pitch brief: {{task}}\n\nBuild a persuasive pitch deck for the idea above. Keep it under 12 slides. Every slide earns its place: one idea per slide, concrete numbers over adjectives, and a clear ask at the end.".into(),
                members: vec![
                    member("business-strategist", "Owns the story arc: problem, insight, solution, ask"),
                    member("product-planner", "Keeps the deck to the smallest shippable story"),
                    member("data-analyst", "Supplies the numbers that earn each slide"),
                    member("copywriter", "Writes sharp slide copy with no filler"),
                    member("slide-designer", "Designs the slide layouts"),
                    member("data-visualizer", "Builds the charts and visual data"),
                    member("evidence-reviewer", "Checks every claim against its source"),
                ],
                topology: Topology::Pipeline,
                lead_expert_id: "business-strategist".into(),
                stages: vec![
                    stage(
                        "positioning",
                        &["business-strategist"],
                        "raw idea",
                        "{thesis, audience, ask}",
                        None,
                    ),
                    stage(
                        "narrative",
                        &["product-planner"],
                        "{thesis, audience, ask}",
                        "slide_outline[] as {slide_n, one_idea}",
                        None,
                    ),
                    stage(
                        "numbers",
                        &["data-analyst"],
                        "slide_outline[]",
                        "verified_figures[] as {figure, source}",
                        None,
                    ),
                    stage(
                        "copy",
                        &["copywriter"],
                        "slide_outline[] + verified_figures[]",
                        "slide_copy[] as {slide_n, headline, body}",
                        None,
                    ),
                    stage(
                        "design",
                        &["slide-designer", "data-visualizer"],
                        "slide_copy[]",
                        "deck spec (layout per slide + chart specs)",
                        None,
                    ),
                    stage(
                        "verify",
                        &["evidence-reviewer"],
                        "deck spec + verified_figures[]",
                        "verdict {pass: bool, failed_claims[]}",
                        Some("numbers"),
                    ),
                ],
            },
            Team {
                id: "data-analysis".into(),
                name: "Data Analysis".into(),
                description: "From raw data to decisions.".into(),
                brief_template: "Analysis brief: {{task}}\n\nAnalyze the data described above. Start by validating data quality and stating your assumptions. Show your working, quantify uncertainty, and finish with the decision the numbers support.".into(),
                members: vec![
                    member("data-analyst", "Runs the analysis and tests the hypotheses"),
                    member("sql-analyst", "Pulls and shapes the data"),
                    member("report-writer", "Writes up the findings and the decision they support"),
                    member("evidence-reviewer", "Checks every claim against its source"),
                ],
                topology: Topology::ReviewLoop,
                lead_expert_id: "data-analyst".into(),
                stages: vec![
                    stage(
                        "extract",
                        &["sql-analyst"],
                        "data description + access",
                        "dataset profile {schema, row_count, quality_issues[], fixes_applied[]}",
                        None,
                    ),
                    stage(
                        "analysis",
                        &["data-analyst"],
                        "dataset profile",
                        "findings[] as {finding, evidence, uncertainty}",
                        None,
                    ),
                    stage(
                        "report",
                        &["report-writer"],
                        "findings[]",
                        "report draft (markdown)",
                        None,
                    ),
                    stage(
                        "verify",
                        &["evidence-reviewer"],
                        "report draft + findings[]",
                        "verdict {pass: bool, failed_claims[]}",
                        Some("analysis"),
                    ),
                ],
            },
            Team {
                id: "code-review".into(),
                name: "Code Review".into(),
                description: "A thorough review crew for any changeset.".into(),
                brief_template: "Review brief: {{task}}\n\nReview the code described above. Report issues ordered by severity with file and line references. Distinguish must-fix defects from style nits, and suggest concrete fixes rather than vague advice.".into(),
                members: vec![
                    member("code-reviewer", "Reviews logic, readability, and API design"),
                    member("security-specialist", "Hunts for injection, auth, and data-exposure issues"),
                    member("test-engineer", "Checks test coverage and edge cases"),
                ],
                topology: Topology::ParallelMerge,
                lead_expert_id: "code-reviewer".into(),
                stages: vec![
                    stage(
                        "review",
                        &["code-reviewer", "security-specialist", "test-engineer"],
                        "changeset (diff + context)",
                        "findings[] as {file, line, severity, issue, fix}",
                        None,
                    ),
                    stage(
                        "merge",
                        &["code-reviewer"],
                        "findings[]",
                        "merged review {deduped, ordered by severity, must_fix[] vs nits[]}",
                        None,
                    ),
                ],
            },
            Team {
                id: "job-hunt".into(),
                name: "Job Hunt".into(),
                description: "Finds openings and writes applications that land.".into(),
                brief_template: "Job hunt brief for: {{task}}\n\nFind matching openings posted in the last two weeks, from verifiable companies only. For each: role, company, why it fits, and a tailored application draft.".into(),
                members: vec![
                    member("research-analyst", "Finds roles matching the candidate's profile"),
                    member("copywriter", "Tailors CVs and cover letters per role"),
                    member("evidence-reviewer", "Checks every claim against the CV"),
                ],
                topology: Topology::ReviewLoop,
                lead_expert_id: "research-analyst".into(),
                stages: vec![
                    stage(
                        "opportunities",
                        &["research-analyst"],
                        "candidate profile",
                        "shortlist[] as {role, company, url, posted_date, why_fits} - only posted within the last two weeks, verifiable companies",
                        None,
                    ),
                    stage(
                        "applications",
                        &["copywriter"],
                        "shortlist[] + CV",
                        "drafts[] as {role, company, tailored_bullets[], cover_note}",
                        None,
                    ),
                    stage(
                        "verify",
                        &["evidence-reviewer"],
                        "drafts[] + CV",
                        "verdict {pass: bool, failed_claims[]} - every claim must trace to the CV, no invented facts",
                        Some("applications"),
                    ),
                ],
            },
        ]
    }

    // ---------------------------------------------------------------------------
    // Tests (deterministic: tempdir, ScriptedWorker, no network/subprocesses)
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod render_brief_tests {
        use super::*;

        #[test]
        fn override_renders_into_template() {
            assert_eq!(
                render_brief("Review brief: {{task}}\n\nReview it.", "src/main.rs").unwrap(),
                "Review brief: src/main.rs\n\nReview it."
            );
        }

        #[test]
        fn placeholder_without_task_is_an_error_not_a_run() {
            let err = render_brief("Review brief: {{task}}", "").unwrap_err();
            assert!(err.contains("task"), "{err}");
        }

        #[test]
        fn plain_template_and_override_keep_old_behavior() {
            assert_eq!(render_brief("Do the thing.", "").unwrap(), "Do the thing.");
            assert_eq!(render_brief("Do the thing.", "other").unwrap(), "other");
            assert!(render_brief("", "").is_err());
        }
    }
}

pub mod templates {
    //! Shared JSON-file storage for the template galleries (teams, experts).
    //!
    //! Each gallery keeps one file per template under
    //! `<data_dir>/<kind>/<id>.json` (`kind` is `"teams"` or `"experts"`).
    //! Ids are slugs (`pantheon_api::ident::is_slug`: ASCII alphanumerics,
    //! `-`, `_`), validated on every read path that takes an id from the URL
    //! so a crafted `:id` can never escape the gallery directory. Writes go
    //! through [`crate::util::atomic_write`]. Seeding is idempotent: bundled
    //! templates are written only when the gallery directory holds no `.json`
    //! files, so a re-seed never overwrites user edits or user-added items.

    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use std::path::{Path, PathBuf};

    /// Slug rule for template ids, shared by teams and experts.
    pub(crate) fn valid_slug(id: &str) -> bool {
        pantheon_api::ident::is_slug(id)
    }

    /// `<data_dir>/<kind>`.
    pub(crate) fn store_dir(data_dir: &Path, kind: &str) -> PathBuf {
        data_dir.join(kind)
    }

    /// Path of one template file. `None` when the id is not a slug - the
    /// caller turns that into a 400 before touching the filesystem. The
    /// `starts_with` guard is defense in depth: a valid slug cannot contain
    /// `/`, `\`, or `..`, so the join cannot escape `store_dir` anyway.
    pub(crate) fn item_path(data_dir: &Path, kind: &str, id: &str) -> Option<PathBuf> {
        if !valid_slug(id) {
            return None;
        }
        let dir = store_dir(data_dir, kind);
        let path = dir.join(format!("{id}.json"));
        if path.starts_with(&dir) {
            Some(path)
        } else {
            None
        }
    }

    /// `true` when the gallery directory holds at least one `.json` file.
    /// A missing directory counts as empty (first access seeds it).
    fn has_items(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.filter_map(|e| e.ok()).any(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("json"))
        })
    }

    /// Read and parse one template file.
    pub(crate) fn read_item<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
        let raw =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
    }

    /// All templates in a gallery, sorted by id. A single corrupt file does
    /// not fail the whole listing - it is skipped, and the gallery stays
    /// usable (the file can be fixed or deleted through the API).
    pub(crate) fn list_items<T: DeserializeOwned>(dir: &Path) -> Result<Vec<(String, T)>, String> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let is_json = path
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("json"));
            if !is_json {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !valid_slug(stem) {
                continue;
            }
            if let Ok(item) = read_item::<T>(&path) {
                out.push((stem.to_string(), item));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Write one template file, creating the gallery directory as needed.
    pub(crate) fn write_item<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(value).map_err(|e| format!("serialize template: {e}"))?;
        crate::util::atomic_write(path, &text)
    }

    /// Delete one template file. `Ok(false)` when there was nothing there
    /// (the caller turns that into a 404).
    pub(crate) fn delete_item(path: &Path) -> Result<bool, String> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("delete {}: {e}", path.display())),
        }
    }

    /// Write the bundled seeds when the gallery is empty. Returns `true`
    /// when seeding happened. Never touches an existing file, so re-seeding
    /// is a no-op and user edits / user-added templates always survive.
    pub(crate) fn seed_if_empty<T: Serialize>(
        data_dir: &Path,
        kind: &str,
        seeds: &[T],
        id_of: impl Fn(&T) -> &str,
    ) -> Result<bool, String> {
        let dir = store_dir(data_dir, kind);
        if has_items(&dir) {
            return Ok(false);
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        for seed in seeds {
            let id = id_of(seed);
            debug_assert!(valid_slug(id), "bundled seed id must be a slug: {id}");
            let path = dir.join(format!("{id}.json"));
            // `create_new`: a concurrent seeder winning the race must not
            // clobber the file it wrote.
            if path.exists() {
                continue;
            }
            let text =
                serde_json::to_string_pretty(seed).map_err(|e| format!("serialize seed: {e}"))?;
            crate::util::atomic_write(&path, &text)
                .map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        Ok(true)
    }
}

pub mod ideas {
    //! Ideas: the HTTP surface for nightly-gated proactive suggestions.
    //!
    //! `GET /api/ideas` lists them; `POST /api/ideas/:id/accept` spawns the
    //! work (a new run for `general` ideas, a scheduled job for
    //! `scheduled_task` ideas); `dismiss` and `feedback` record the user's
    //! signals so nightly generation can tune itself.

    use crate::{bad_json, conflict, err_json, json_ok, App};
    use pantheon_api::logging::now_ms;
    use pantheon_gateway::http::{Request, Response};
    use pantheon_storage::{FeedbackSignal, Idea, IdeaKind, IdeaStatus, IdeaStore};
    use std::collections::HashMap;

    fn store(app: &App) -> Result<IdeaStore, Response> {
        IdeaStore::open(&app.data_dir)
            .map_err(|e| err_json(500, "IDEA", &format!("open ideas store: {e}")))
    }

    fn idea_json(i: &Idea) -> serde_json::Value {
        let mut v = serde_json::json!({
            "id": i.id,
            "title": i.title,
            "description": i.description,
            "includes": i.includes,
            "kind": match i.kind {
                IdeaKind::General => "general",
                IdeaKind::ScheduledTask => "scheduled_task",
            },
            "status": match i.status {
                IdeaStatus::Pending => "pending",
                IdeaStatus::Accepted => "accepted",
                IdeaStatus::Dismissed => "dismissed",
                IdeaStatus::Done => "done",
            },
            "created_day": i.created_day,
        });
        // The schedule spec rides only on scheduled_task ideas, exactly the
        // shape the app renders.
        if let Some(s) = &i.schedule {
            v["schedule"] = serde_json::json!({
                "cron": s.cron,
                "deliver": s.deliver,
                "prompt": s.prompt,
            });
        }
        v
    }

    /// `GET /api/ideas`: all ideas, pending first, then newest day first.
    pub fn list(app: &App) -> Response {
        let store = match store(app) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let ideas = match store.list() {
            Ok(v) => v,
            Err(e) => return err_json(500, "IDEA", &format!("list ideas: {e}")),
        };
        let mut ideas = ideas;
        ideas.sort_by(|a, b| {
            let pa = a.status == IdeaStatus::Pending;
            let pb = b.status == IdeaStatus::Pending;
            pb.cmp(&pa).then(b.created_day.cmp(&a.created_day))
        });
        json_ok(serde_json::json!({
            "ideas": ideas.iter().map(idea_json).collect::<Vec<_>>(),
        }))
    }

    /// Load a pending idea or answer 404/409.
    fn pending_idea(app: &App, id: &str) -> Result<(IdeaStore, Idea), Response> {
        let store = store(app)?;
        let idea = match store.get(id) {
            Ok(Some(i)) => i,
            Ok(None) => return Err(err_json(404, "NOT_FOUND", "unknown idea")),
            Err(e) => return Err(err_json(500, "IDEA", &format!("get idea: {e}"))),
        };
        if idea.status != IdeaStatus::Pending {
            return Err(conflict(
                "IDEA_STATE",
                "idea is no longer pending; only pending ideas can be accepted or dismissed",
            ));
        }
        Ok((store, idea))
    }

    /// Fabricate the request `runs::create` expects, reusing its exact
    /// admit-run + spawn-turn path instead of reimplementing it.
    fn create_run_request(message: &str, title: &str) -> Request {
        Request {
            method: "POST".into(),
            path: "/api/runs".into(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: serde_json::json!({"message": message, "title": title})
                .to_string()
                .into_bytes(),
        }
    }

    fn buffered_body(resp: &Response) -> Option<(u16, serde_json::Value)> {
        match resp {
            Response::Buffered { status, body, .. } => {
                Some((*status, serde_json::from_slice(body).unwrap_or_default()))
            }
            _ => None,
        }
    }

    /// `POST /api/ideas/:id/accept`: spawn the work behind the idea.
    /// `general` → a new run carrying the idea's plan; `scheduled_task` →
    /// a scheduled job built from the proposed spec.
    pub fn accept(app: &App, id: &str) -> Response {
        let (store, idea) = match pending_idea(app, id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        match idea.kind {
            IdeaKind::General => {
                let plan = format!(
                    "Idea: {}\n\n{}\n\nPlan:\n{}",
                    idea.title,
                    truncate(&idea.description, 2000),
                    idea.includes
                        .iter()
                        .map(|s| format!("- {s}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                let resp = crate::runs::create(app, &create_run_request(&plan, &idea.title));
                let (status, body) = match buffered_body(&resp) {
                    Some(v) => v,
                    None => {
                        return err_json(500, "IDEA", "run creation returned a streaming response");
                    }
                };
                if status != 201 {
                    // The run never started: leave the idea pending so the
                    // user can retry. Propagate the real error shape.
                    return resp;
                }
                let run_id = body
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if let Err(e) = finalize(&store, &idea, true) {
                    return err_json(500, "IDEA", &format!("mark accepted: {e}"));
                }
                json_ok(serde_json::json!({
                    "ok": true,
                    "status": "accepted",
                    "run_id": run_id,
                }))
            }
            IdeaKind::ScheduledTask => {
                let spec = match &idea.schedule {
                    Some(s) => s,
                    None => return bad_json("scheduled_task idea has no schedule spec"),
                };
                let body = serde_json::json!({
                    "task": spec.prompt,
                    "cron": spec.cron,
                    "deliver": spec.deliver,
                });
                let req = Request {
                    method: "POST".into(),
                    path: "/api/schedule/jobs".into(),
                    query: HashMap::new(),
                    headers: HashMap::new(),
                    body: body.to_string().into_bytes(),
                };
                // The shared schedule creation path: template expansion,
                // cron + deliver validation, the locked read-modify-write on
                // schedule.json. A 400 here (e.g. bad cron) leaves the idea
                // pending so the user can retry.
                let resp = crate::schedule::create_job(app, &req);
                let (status, v) = match buffered_body(&resp) {
                    Some(v) => v,
                    None => {
                        return err_json(500, "IDEA", "job creation returned a streaming response");
                    }
                };
                if status != 200 {
                    return resp;
                }
                let schedule_id = v
                    .get("job")
                    .and_then(|j| j.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if let Err(e) = finalize(&store, &idea, true) {
                    return err_json(500, "IDEA", &format!("mark accepted: {e}"));
                }
                json_ok(serde_json::json!({
                    "ok": true,
                    "status": "accepted",
                    "schedule_id": schedule_id,
                }))
            }
        }
    }

    /// `POST /api/ideas/:id/dismiss`: record the dismissal and its topic
    /// signal (generation downranks repeatedly dismissed topics).
    pub fn dismiss(app: &App, id: &str) -> Response {
        let (store, idea) = match pending_idea(app, id) {
            Ok(v) => v,
            Err(r) => return r,
        };
        if let Err(e) = finalize(&store, &idea, false) {
            return err_json(500, "IDEA", &format!("mark dismissed: {e}"));
        }
        json_ok(serde_json::json!({"ok": true, "status": "dismissed"}))
    }

    /// `POST /api/ideas/:id/feedback` with `{"signal":"more"}` or
    /// `{"signal":"less"}`: explicit tuning feedback on the idea and topic.
    pub fn feedback(app: &App, id: &str, req: &Request) -> Response {
        let store = match store(app) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let body: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => return bad_json(&format!("invalid JSON body: {e}")),
        };
        let signal = match body.get("signal").and_then(|v| v.as_str()) {
            Some("more") => FeedbackSignal::More,
            Some("less") => FeedbackSignal::Less,
            _ => return bad_json("field \"signal\" must be \"more\" or \"less\""),
        };
        match store.record_feedback(id, signal) {
            Ok(true) => json_ok(serde_json::json!({"ok": true})),
            Ok(false) => err_json(404, "NOT_FOUND", "unknown idea"),
            Err(e) => err_json(500, "IDEA", &format!("record feedback: {e}")),
        }
    }

    /// Durably mark the idea accepted/dismissed and bump its topic signal.
    fn finalize(
        store: &IdeaStore,
        idea: &Idea,
        accepted: bool,
    ) -> Result<(), pantheon_api::error::PantheonError> {
        let status = if accepted {
            IdeaStatus::Accepted
        } else {
            IdeaStatus::Dismissed
        };
        store.set_status(&idea.id, status, Some(now_ms()))?;
        store.bump_topic(&idea.topic, accepted)?;
        Ok(())
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

pub mod profiles {
    //! Agent profile persona-file manager.
    //!
    //! The profile agent is the one with a personality: its `soul_file`
    //! (SOUL.md), `user_file` (USER.md), and `agents_file` (AGENTS.md) are
    //! injected into the main session's system prompt every turn. These
    //! endpoints let the dashboard and the mobile app read and edit those
    //! files without shell access.
    //!
    //! - `GET /api/profiles/:name/files` →
    //!   `{soul: {path, content}, user: {path, content}, agents: {path, content}}`.
    //!   A file that is not declared or cannot be read reports
    //!   `{path: null, content: ""}`.
    //! - `PUT /api/profiles/:name/files` ←
    //!   `{file: "soul"|"user"|"agents", content: String}`. Writes the content
    //!   to the profile's declared path. When the profile declares no path for
    //!   that file, the file is created at
    //!   `<data_dir>/profiles/<name>/{SOUL,USER,AGENTS}.md` and the profile
    //!   field is set through the same validated config write path
    //!   `PUT /api/config` uses.
    //!
    //! Security: the request never chooses a path. Writes go only to (a) the
    //! profile's declared file paths (operator-configured, used as-is) or (b)
    //! the `<data_dir>/profiles/<name>/` directory. The `:name` segment is
    //! rejected when it could escape that directory (`/`, `\`, `..`).
    //! Content is capped at 256 KiB.

    use crate::util::atomic_write;
    use crate::{bad_json, body_json, err_json, json_ok, App};
    use pantheon_gateway::http::{Request, Response};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// Persona-file content cap: 256 KiB. These are prompt-injected every turn;
    /// anything larger is a mistake, not a persona.
    const MAX_FILE_BYTES: usize = 256 * 1024;

    /// Map the `file` discriminator to (config field, default file name).
    fn file_slot(kind: &str) -> Option<(&'static str, &'static str)> {
        match kind {
            "soul" => Some(("soul_file", "SOUL.md")),
            "user" => Some(("user_file", "USER.md")),
            "agents" => Some(("agents_file", "AGENTS.md")),
            _ => None,
        }
    }

    /// Read `config.toml` as a TOML value. Kept local so this module does not
    /// depend on config.rs internals.
    fn read_config_doc(data_dir: &Path) -> Result<toml::Value, Response> {
        let path = data_dir.join("config.toml");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| err_json(404, "PROFILES", &format!("read {}: {e}", path.display())))?;
        toml::from_str::<toml::Value>(&raw)
            .map_err(|e| err_json(400, "PROFILES", &format!("config.toml does not parse: {e}")))
    }

    /// The `[agents.<name>]` table, or a 404 when the profile is not declared.
    fn profile_table<'a>(doc: &'a toml::Value, name: &str) -> Result<&'a toml::Value, Response> {
        doc.get("agents").and_then(|v| v.get(name)).ok_or_else(|| {
            err_json(
                404,
                "PROFILES",
                &format!("no [agents.{name}] profile declared"),
            )
        })
    }

    /// Declared path for one persona-file field; blank reads as undeclared.
    fn declared_path(table: &toml::Value, field: &str) -> Option<String> {
        table
            .get(field)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// Reject profile names that could escape `<data_dir>/profiles/<name>/`.
    fn valid_name(name: &str) -> bool {
        !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.contains("..")
    }

    /// `GET /api/profiles/:name/files`.
    pub fn get_files(app: &App, name: &str) -> Response {
        if !valid_name(name) {
            return bad_json("invalid profile name");
        }
        let doc = match read_config_doc(&app.data_dir) {
            Ok(d) => d,
            Err(r) => return r,
        };
        let table = match profile_table(&doc, name) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let mut out = serde_json::Map::new();
        for (kind, (field, _)) in [
            ("soul", file_slot("soul").unwrap()),
            ("user", file_slot("user").unwrap()),
            ("agents", file_slot("agents").unwrap()),
        ] {
            let (path, content) = match declared_path(table, field) {
                Some(p) => match std::fs::read_to_string(&p) {
                    Ok(c) => (Some(p), c),
                    Err(_) => (None, String::new()),
                },
                None => (None, String::new()),
            };
            out.insert(
                kind.to_string(),
                serde_json::json!({ "path": path, "content": content }),
            );
        }
        json_ok(serde_json::Value::Object(out))
    }

    /// Resolve the write target: the declared path when the profile has one,
    /// otherwise `<data_dir>/profiles/<name>/<default>`. Returns the target
    /// and whether the config field needs setting.
    fn write_target(
        data_dir: &Path,
        name: &str,
        table: &toml::Value,
        field: &str,
        default_name: &str,
    ) -> Result<(PathBuf, bool), Response> {
        if let Some(p) = declared_path(table, field) {
            return Ok((PathBuf::from(p), false));
        }
        let dir = data_dir.join("profiles").join(name);
        std::fs::create_dir_all(&dir)
            .map_err(|e| err_json(500, "PROFILES", &format!("create {}: {e}", dir.display())))?;
        Ok((dir.join(default_name), true))
    }

    /// `PUT /api/profiles/:name/files` ← `{file, content}`.
    pub fn put_files(app: &App, req: &Request, name: &str) -> Response {
        if !valid_name(name) {
            return bad_json("invalid profile name");
        }
        let body = match body_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let kind = body.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let (field, default_name) = match file_slot(kind) {
            Some(f) => f,
            None => return bad_json("file must be one of: soul, user, agents"),
        };
        let content = match body.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return bad_json("content is required"),
        };
        if content.len() > MAX_FILE_BYTES {
            return err_json(413, "PROFILES", "content exceeds the 256 KiB cap");
        }
        let doc = match read_config_doc(&app.data_dir) {
            Ok(d) => d,
            Err(r) => return r,
        };
        let table = match profile_table(&doc, name) {
            Ok(t) => t,
            Err(r) => return r,
        };
        let (target, needs_config) =
            match write_target(&app.data_dir, name, table, field, default_name) {
                Ok(t) => t,
                Err(r) => return r,
            };
        if let Err(e) = atomic_write(&target, content) {
            return err_json(500, "PROFILES", &format!("write: {e}"));
        }
        if needs_config {
            // Point the profile at the new file through the validated config
            // write path (same validation `PUT /api/config` applies).
            let mut changes = BTreeMap::new();
            changes.insert(
                format!("agents.{name}.{field}"),
                toml::Value::String(target.display().to_string()),
            );
            return crate::config::apply_changes(app, &changes, true);
        }
        json_ok(serde_json::json!({
            "ok": true,
            "path": target.display().to_string(),
        }))
    }

    /* ------------------------------------------------------------------ */
    /* Profile DELETE (added separately from the persona-file manager       */
    /* above): remove a whole `[agents.<name>]` profile.                    */
    /* ------------------------------------------------------------------ */

    /// `DELETE /api/profiles/:name` → removes the `[agents.<name>]` table
    /// through the same config write path `PUT /api/config` uses
    /// ([`crate::config::remove_paths`]).
    ///
    /// - 404 when no `[agents.<name>]` profile is declared.
    /// - 409 when `<name>` is the active profile (`agent = "<name>"` in the
    ///   config): the active profile cannot be deleted.
    /// - The profile's persona files under `<data_dir>/profiles/<name>/` are
    ///   removed too - and only those. Declared paths pointing anywhere
    ///   outside that directory are left untouched.
    pub fn delete_profile(app: &App, name: &str) -> Response {
        if !valid_name(name) {
            return bad_json("invalid profile name");
        }
        let doc = match read_config_doc(&app.data_dir) {
            Ok(d) => d,
            Err(r) => return r,
        };
        if let Err(r) = profile_table(&doc, name) {
            return r;
        }
        let active = doc.get("agent").and_then(|v| v.as_str()).unwrap_or("");
        if active == name {
            return err_json(
                409,
                "PROFILES",
                &format!(
                    "cannot delete the active profile '{name}'; \
                     set `agent` to another profile first"
                ),
            );
        }
        let path = format!("agents.{name}");
        if let Err(r) = crate::config::remove_paths(app, &[&path]) {
            return r;
        }
        // Persona files live only under `<data_dir>/profiles/<name>/`. The
        // name was validated above (no `/`, `\`, or `..`), so this join
        // cannot escape that directory; the starts_with guard is defense in
        // depth.
        let base = app.data_dir.join("profiles");
        let dir = base.join(name);
        if dir.starts_with(&base) {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return err_json(500, "PROFILES", &format!("remove {}: {e}", dir.display()))
                }
            }
        }
        json_ok(serde_json::json!({"ok": true, "deleted": name}))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// read_config_doc is the production TOML-document parse this file
        /// does; the toml 1.x migration showed such sites can silently change
        /// meaning (`str.parse::<toml::Value>()` became a single-value
        /// parse), so the round-trip is pinned here.
        #[test]
        fn read_config_doc_parses_documents_and_rejects_garbage() {
            let d = std::env::temp_dir().join(format!(
                "pantheon-dash-profiles-test-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("config.toml"),
                "[model]\nprovider = \"openai\"\n\n[agents.coder]\npersona = \"coder.md\"\n",
            )
            .unwrap();
            let doc = match read_config_doc(&d) {
                Ok(v) => v,
                Err(_) => panic!("document config must parse"),
            };
            let table = match profile_table(&doc, "coder") {
                Ok(t) => t,
                Err(_) => panic!("profile present"),
            };
            assert_eq!(
                declared_path(table, "persona"),
                Some("coder.md".to_string())
            );
            assert!(profile_table(&doc, "missing").is_err());

            std::fs::write(d.join("config.toml"), "not [ toml").unwrap();
            assert!(
                read_config_doc(&d).is_err(),
                "garbage must be a 400, not a panic or silent Ok"
            );
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}
