//! Teams + experts gallery behavior, exercised over real HTTP.
//!
//! Distilled from the dashboard's handler tests: CRUD round-trips, seed
//! idempotency (first-access seeding never overwrites user edits), and
//! fail-closed `use` paths (unknown profile refs and dangling expert
//! refs are 400 and nothing spawns).

#[path = "support.rs"]
mod support;

use support::{boot, Dash, Resp};

fn team_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": "Sample",
        "description": "desc",
        "brief_template": "Do {{task}} well.",
        "members": [
            {"expert_id": "data-analyst", "role": "leads", "profile": "default"},
            {"expert_id": "sql-analyst", "role": "helps", "profile": "default"}
        ],
        "topology": "pipeline",
        "lead_expert_id": "data-analyst",
        "stages": [
            {"name": "gather", "members": ["data-analyst"],
             "input_contract": "raw request", "output_contract": "dataset profile",
             "loop_back_to": null},
            {"name": "shape", "members": ["sql-analyst"],
             "input_contract": "dataset profile", "output_contract": "query result",
             "loop_back_to": null}
        ]
    })
}

fn expert_json(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": "Sample",
        "description": "desc",
        "color": "#7C3AED",
        "icon": "star",
        "persona": "You are a sample expert. Be brief."
    })
}

fn post_json(d: &Dash, path: &str, v: &serde_json::Value) -> Resp {
    d.post(path, &v.to_string())
}

// ---------------------------------------------------------------------------
// Teams
// ---------------------------------------------------------------------------

#[test]
fn teams_crud_round_trip() {
    let d = boot();
    // Create (also triggers seeding).
    let r = post_json(&d, "/api/teams", &team_json("mine"));
    assert_eq!(r.status, 201, "{}", r.body);
    let b = r.json();
    assert_eq!(b["id"], "mine");
    assert_eq!(b["member_count"], 2);
    // Get: members enriched with their expert's display identity.
    let r = d.get("/api/teams/mine");
    assert_eq!(r.status, 200, "{}", r.body);
    let b = r.json();
    assert_eq!(b["name"], "Sample");
    assert_eq!(b["members"][0]["expert_id"], "data-analyst");
    assert_eq!(b["members"][0]["expert"]["name"], "Data Analyst");
    // List: the 5 seeds plus ours.
    let r = d.get("/api/teams");
    assert_eq!(r.status, 200, "{}", r.body);
    let teams = r.json()["teams"].as_array().unwrap().clone();
    assert_eq!(teams.len(), 6, "{teams:?}");
    assert!(teams.iter().any(|t| t["id"] == "deep-research"));
    assert!(teams.iter().any(|t| t["id"] == "mine"));
    // Update: the URL id wins over a disagreeing body id.
    let mut changed = team_json("other-id");
    changed["name"] = serde_json::json!("Renamed");
    let r = d.put("/api/teams/mine", &changed.to_string());
    assert_eq!(r.status, 200, "{}", r.body);
    let b = r.json();
    assert_eq!(b["id"], "mine");
    assert_eq!(b["name"], "Renamed");
    // Delete, then 404s.
    let r = d.delete("/api/teams/mine");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["deleted"], "mine");
    assert_eq!(d.get("/api/teams/mine").status, 404);
    assert_eq!(d.delete("/api/teams/mine").status, 404);
    assert_eq!(
        d.put("/api/teams/mine", &team_json("mine").to_string())
            .status,
        404
    );
}

#[test]
fn teams_create_rejects_bad_input() {
    let d = boot();
    for bad_id in ["", "has space", "a/b", "..", "x.json"] {
        let r = post_json(&d, "/api/teams", &team_json(bad_id));
        assert_eq!(r.status, 400, "id {bad_id:?}: {}", r.body);
    }
    let mut t = team_json("t1");
    t["name"] = serde_json::json!("  ");
    assert_eq!(post_json(&d, "/api/teams", &t).status, 400);
    let mut t = team_json("t2");
    t["members"] = serde_json::json!([]);
    assert_eq!(post_json(&d, "/api/teams", &t).status, 400);
    let mut t = team_json("t3");
    t["members"] = serde_json::json!((0..9)
        .map(|_| serde_json::json!(
        {"expert_id": "data-analyst", "role": "r", "profile": "default"}))
        .collect::<Vec<_>>());
    assert_eq!(post_json(&d, "/api/teams", &t).status, 400);
    // Dangling expert ref.
    let mut t = team_json("t4");
    t["members"][0]["expert_id"] = serde_json::json!("nope");
    assert_eq!(post_json(&d, "/api/teams", &t).status, 400);
    // Bad expert_id slug.
    let mut t = team_json("t5");
    t["members"][0]["expert_id"] = serde_json::json!("has space");
    assert_eq!(post_json(&d, "/api/teams", &t).status, 400);
    // Blank profile.
    let mut t = team_json("t6");
    t["members"][0]["profile"] = serde_json::json!(" ");
    assert_eq!(post_json(&d, "/api/teams", &t).status, 400);
    // Invalid JSON body.
    assert_eq!(d.post("/api/teams", "{oops").status, 400);
}

#[test]
fn teams_create_duplicate_is_409() {
    let d = boot();
    assert_eq!(post_json(&d, "/api/teams", &team_json("dup")).status, 201);
    let r = post_json(&d, "/api/teams", &team_json("dup"));
    assert_eq!(r.status, 409, "{}", r.body);
}

#[test]
fn teams_id_escape_rejected() {
    let d = boot();
    // ".." reaches the handler as an id and is rejected before any
    // filesystem access; multi-segment paths never match a route at all.
    // Either way the outcome must be 400/404 - never a file read.
    assert_eq!(d.get("/api/teams/..").status, 400);
    assert_eq!(d.delete("/api/teams/..").status, 400);
    assert_eq!(d.get("/api/teams/%2e%2e").status, 400);
    for evil in ["../x", "a/b"] {
        for r in [
            d.get(&format!("/api/teams/{evil}")),
            d.delete(&format!("/api/teams/{evil}")),
        ] {
            assert!(r.status == 400 || r.status == 404, "{evil:?}: {}", r.status);
        }
    }
}

#[test]
fn teams_seed_idempotency() {
    let d = boot();
    // First access seeds exactly 5.
    let r = d.get("/api/teams");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["teams"].as_array().unwrap().len(), 5);
    // A user edit to a seed survives re-seeding (edit via the public
    // update path: GET, tweak, PUT back).
    let mut seed = d.get("/api/teams/deep-research").json();
    seed["description"] = serde_json::json!("user edited");
    let r = d.put("/api/teams/deep-research", &seed.to_string());
    assert_eq!(r.status, 200, "{}", r.body);
    // A user-added team survives too.
    assert_eq!(
        post_json(&d, "/api/teams", &team_json("custom")).status,
        201
    );
    // Second access (re-seed): still 6, edits intact.
    let r = d.get("/api/teams");
    assert_eq!(r.status, 200, "{}", r.body);
    let teams = r.json()["teams"].as_array().unwrap().clone();
    assert_eq!(teams.len(), 6, "{teams:?}");
    let edited = teams.iter().find(|t| t["id"] == "deep-research").unwrap();
    assert_eq!(edited["description"], "user edited");
    assert!(teams.iter().any(|t| t["id"] == "custom"));
}

#[test]
fn teams_create_rejects_dangling_expert() {
    let d = boot();
    let mut t = team_json("dangling");
    t["members"][0]["expert_id"] = serde_json::json!("ghost-expert");
    t["lead_expert_id"] = serde_json::json!("ghost-expert");
    t["stages"][0]["members"] = serde_json::json!(["ghost-expert"]);
    let r = post_json(&d, "/api/teams", &t);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("ghost-expert"), "{}", r.body);
}

#[test]
fn teams_create_rejects_bad_stage_refs() {
    let d = boot();
    // Stage member not on the team.
    let mut t = team_json("badstage");
    t["stages"][0]["members"] = serde_json::json!(["data-analyst", "ghost-expert"]);
    let r = post_json(&d, "/api/teams", &t);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("not a team member"), "{}", r.body);
    // Loop-back to an unknown stage.
    let mut t = team_json("badloop");
    t["stages"][1]["loop_back_to"] = serde_json::json!("nope");
    let r = post_json(&d, "/api/teams", &t);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("unknown stage"), "{}", r.body);
    // Loop-back to a *later* stage: unexecutable.
    let mut t = team_json("fwdloop");
    t["stages"][0]["loop_back_to"] = serde_json::json!("shape");
    let r = post_json(&d, "/api/teams", &t);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("not an earlier stage"), "{}", r.body);
}

#[test]
fn teams_use_unknown_profile_rejected_and_nothing_spawns() {
    let d = boot();
    let mut t = team_json("bogus");
    t["members"][0]["profile"] = serde_json::json!("nope");
    assert_eq!(post_json(&d, "/api/teams", &t).status, 201);
    let r = d.post("/api/teams/bogus/use", r#"{"judge": false}"#);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(
        r.body.contains("nope"),
        "error names the bad profile: {}",
        r.body
    );
    // Nothing spawned: the orchestrator's first id is still unknown.
    assert!(d.swarm.status("sw_1").is_err());
}

#[test]
fn teams_use_rejects_dangling_expert_and_nothing_spawns() {
    let d = boot();
    assert_eq!(post_json(&d, "/api/teams", &team_json("crew3")).status, 201);
    // Delete the expert out from under the team: the team file still
    // references it, so `use` must fail closed.
    assert_eq!(d.delete("/api/experts/data-analyst").status, 200);
    let r = d.post("/api/teams/crew3/use", r#"{"judge": false}"#);
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("data-analyst"), "{}", r.body);
    // Nothing spawned: the orchestrator's first id is still unknown.
    assert!(d.swarm.status("sw_1").is_err());
    // And the gallery marks the member unresolved instead of crashing.
    let b = d.get("/api/teams/crew3").json();
    assert_eq!(b["members"][0]["expert"], serde_json::Value::Null);
    assert_eq!(b["members"][0]["expert_id"], "data-analyst");
}

#[test]
fn teams_use_missing_team_is_404() {
    let d = boot();
    assert_eq!(
        d.post("/api/teams/nope/use", r#"{"judge": false}"#).status,
        404
    );
}

#[test]
fn teams_use_empty_task_is_400() {
    let d = boot();
    let mut t = team_json("notask");
    t["brief_template"] = serde_json::json!("   ");
    assert_eq!(post_json(&d, "/api/teams", &t).status, 201);
    assert_eq!(
        d.post("/api/teams/notask/use", r#"{"judge": false}"#)
            .status,
        400
    );
}

#[test]
fn teams_use_spawns_swarm_with_response_shape() {
    let d = boot();
    d.write_data_file(
        "config.toml",
        "[agents.nyx]\ndisplay_name = \"Nyx\"\n\n[agents.atlas]\ndisplay_name = \"Atlas\"\n",
    );
    let mut t = team_json("crew");
    t["members"][0]["profile"] = serde_json::json!("nyx");
    t["members"][1]["profile"] = serde_json::json!("atlas");
    assert_eq!(post_json(&d, "/api/teams", &t).status, 201);
    // Task override wins over the brief template.
    let r = d.post(
        "/api/teams/crew/use",
        r#"{"task": "ship it", "judge": false}"#,
    );
    assert_eq!(r.status, 201, "{}", r.body);
    let b = r.json();
    assert_eq!(b["ok"], true);
    let swarm_id = b["swarm_id"].as_str().unwrap().to_string();
    assert!(swarm_id.starts_with("sw_"), "{b}");
    assert_eq!(b["id"], b["swarm_id"]);
    let run_id = b["run_id"].as_str().unwrap();
    assert!(!run_id.is_empty(), "{b}");
    // Staged: lead plus the current (first) stage's members - the lead
    // is also the gather member here, so it appears twice.
    let names: Vec<&str> = b["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(names, ["Data Analyst", "Data Analyst"]);
    assert_eq!(b["status_view"]["task"], "ship it");
    // The orchestrator really holds the swarm (fail-closed tests above
    // prove a rejected `use` leaves it untouched).
    assert!(d.swarm.status(&swarm_id).is_ok());
}

// ---------------------------------------------------------------------------
// Experts
// ---------------------------------------------------------------------------

#[test]
fn experts_crud_round_trip() {
    let d = boot();
    let r = post_json(&d, "/api/experts", &expert_json("mine"));
    assert_eq!(r.status, 201, "{}", r.body);
    let b = r.json();
    assert_eq!(b["id"], "mine");
    assert_eq!(b["persona"], "You are a sample expert. Be brief.");

    let r = d.get("/api/experts/mine");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["name"], "Sample");

    let r = d.get("/api/experts");
    assert_eq!(r.status, 200, "{}", r.body);
    let experts = r.json()["experts"].as_array().unwrap().clone();
    assert_eq!(experts.len(), 28, "27 seeds + 1");
    assert!(experts.iter().any(|e| e["id"] == "sql-analyst"));

    let mut changed = expert_json("other-id");
    changed["name"] = serde_json::json!("Renamed");
    let r = d.put("/api/experts/mine", &changed.to_string());
    assert_eq!(r.status, 200, "{}", r.body);
    let b = r.json();
    assert_eq!(b["id"], "mine");
    assert_eq!(b["name"], "Renamed");

    let r = d.delete("/api/experts/mine");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["deleted"], "mine");
    assert_eq!(d.get("/api/experts/mine").status, 404);
    assert_eq!(d.delete("/api/experts/mine").status, 404);
    assert_eq!(
        d.put("/api/experts/mine", &expert_json("mine").to_string())
            .status,
        404
    );
}

#[test]
fn experts_create_rejects_bad_input() {
    let d = boot();
    for bad_id in ["", "has space", "a/b", ".."] {
        let r = post_json(&d, "/api/experts", &expert_json(bad_id));
        assert_eq!(r.status, 400, "id {bad_id:?}: {}", r.body);
    }
    let mut e = expert_json("e1");
    e["name"] = serde_json::json!(" ");
    assert_eq!(post_json(&d, "/api/experts", &e).status, 400);
    let mut e = expert_json("e2");
    e["color"] = serde_json::json!("blue");
    assert_eq!(post_json(&d, "/api/experts", &e).status, 400);
    let mut e = expert_json("e3");
    e["persona"] = serde_json::json!("   ");
    assert_eq!(post_json(&d, "/api/experts", &e).status, 400);
    let mut e = expert_json("e4");
    e["icon"] = serde_json::json!("");
    assert_eq!(post_json(&d, "/api/experts", &e).status, 400);
    // Duplicate id.
    assert_eq!(
        post_json(&d, "/api/experts", &expert_json("dup")).status,
        201
    );
    assert_eq!(
        post_json(&d, "/api/experts", &expert_json("dup")).status,
        409
    );
}

#[test]
fn experts_id_escape_rejected() {
    let d = boot();
    assert_eq!(d.get("/api/experts/..").status, 400);
    assert_eq!(d.delete("/api/experts/..").status, 400);
    assert_eq!(d.get("/api/experts/%2e%2e").status, 400);
    for evil in ["../x", "a/b"] {
        for r in [
            d.get(&format!("/api/experts/{evil}")),
            d.delete(&format!("/api/experts/{evil}")),
        ] {
            assert!(r.status == 400 || r.status == 404, "{evil:?}: {}", r.status);
        }
    }
}

#[test]
fn experts_seed_idempotency() {
    let d = boot();
    let r = d.get("/api/experts");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["experts"].as_array().unwrap().len(), 27);
    // A user edit to a seed survives re-seeding.
    let mut seed = d.get("/api/experts/sql-analyst").json();
    seed["description"] = serde_json::json!("user edited");
    let r = d.put("/api/experts/sql-analyst", &seed.to_string());
    assert_eq!(r.status, 200, "{}", r.body);
    // A user-added expert survives too.
    assert_eq!(
        post_json(&d, "/api/experts", &expert_json("custom")).status,
        201
    );
    // Re-seed: still 28, edits intact.
    let r = d.get("/api/experts");
    assert_eq!(r.status, 200, "{}", r.body);
    let experts = r.json()["experts"].as_array().unwrap().clone();
    assert_eq!(experts.len(), 28);
    let edited = experts.iter().find(|e| e["id"] == "sql-analyst").unwrap();
    assert_eq!(edited["description"], "user edited");
    assert!(experts.iter().any(|e| e["id"] == "custom"));
}

#[test]
fn experts_use_unknown_id_is_404() {
    let d = boot();
    assert_eq!(
        d.post("/api/experts/ghost/use", r#"{"message": "hi"}"#)
            .status,
        404
    );
}

#[test]
fn experts_use_empty_persona_is_400() {
    let d = boot();
    // Hand-write a degenerate expert file (bypasses write-time
    // validation); `use` must still fail closed. The gallery is
    // non-empty, so no seeding clobbers it.
    d.write_data_file(
        "experts/hollow.json",
        r##"{"id":"hollow","name":"Hollow","description":"desc","color":"#ffffff","icon":"x","persona":"  "}"##,
    );
    let r = d.post("/api/experts/hollow/use", "{}");
    assert_eq!(r.status, 400, "{}", r.body);
}

// ---------------------------------------------------------------------------
// Mount-level method gate (was lib.rs `mount_tests`)
// ---------------------------------------------------------------------------

#[test]
fn dashboard_patch_method_reaches_dispatch() {
    let d = boot();
    // PATCH reaches the route arm: unknown run -> 404 from the handler,
    // proving PATCH is dispatched rather than rejected as a method.
    let r = d.patch("/api/runs/nope/queue/0", r#"{"text":"x"}"#);
    assert_eq!(r.status, 404, "{}", r.body);
    // Unknown methods never reach the mount: the gateway rejects them
    // while parsing the request head.
    let r = d.request("OPTIONS", "/api/runs/nope/queue/0", Some(""));
    assert_eq!(r.status, 400, "{}", r.body);
}
