//! `pantheon swarm` — coordinate multiple agent runs toward a shared objective.
//!
//! Architecture (§3): a swarm creates N independent Runs, each with an
//! optional role assignment. The runtime enforces concurrency, token, and
//! cost caps via pantheon_swarm::Caps. A final synthesis Run merges findings.

use pantheon_runtime::{new_run_id, Supervisor};
use pantheon_swarm::{Caps, Swarm};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default agent roles assigned by count. The user can override with explicit
/// roles: pantheon swarm 5 "task" researcher critic verifier analyst synthesizer
const DEFAULT_ROLES: &[&str] = &[
    "researcher",
    "researcher",
    "verifier",
    "critic",
    "synthesizer",
];

/// Durable swarm manifest. Written at spawn time under
/// `<data_dir>/swarms/<swarm_id>.json` so `swarm list` / `swarm status`
/// work across CLI processes. Per-agent liveness still comes from the
/// ledger (each agent owns a run); the manifest is the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SwarmRecord {
    swarm_id: String,
    task: String,
    roles: Vec<String>,
    agents: Vec<SwarmAgent>,
    delivery: Option<String>,
    created_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SwarmAgent {
    label: String,
    run_id: String,
    role: String,
}

fn swarms_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("swarms")
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn save_swarm(data_dir: &Path, record: &SwarmRecord) {
    let dir = swarms_dir(data_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{}.json", record.swarm_id));
    if let Ok(raw) = serde_json::to_string_pretty(record) {
        let _ = std::fs::write(path, raw);
    }
}

fn load_all_swarms(data_dir: &Path) -> Vec<SwarmRecord> {
    let dir = swarms_dir(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Ok(rec) = serde_json::from_str::<SwarmRecord>(&raw) {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then(b.swarm_id.cmp(&a.swarm_id))
    });
    out
}

/// Resolve a full or prefix swarm id against stored manifests.
/// Exact match wins; a unique prefix resolves; several matches error.
fn resolve_swarm(data_dir: &Path, id: &str) -> Result<Option<SwarmRecord>, String> {
    let all = load_all_swarms(data_dir);
    if let Some(rec) = all.iter().find(|r| r.swarm_id == id) {
        return Ok(Some(rec.clone()));
    }
    let hits: Vec<&SwarmRecord> = all.iter().filter(|r| r.swarm_id.starts_with(id)).collect();
    match hits.len() {
        0 => Ok(None),
        1 => Ok(Some(hits[0].clone())),
        _ => {
            let mut msg = format!("ambiguous swarm id '{id}' matches:");
            for h in hits.iter().take(5) {
                msg.push_str(&format!("\n  {}", h.swarm_id));
            }
            Err(msg)
        }
    }
}
fn default_roles(n: usize) -> Vec<&'static str> {
    if n <= DEFAULT_ROLES.len() {
        DEFAULT_ROLES[..n].to_vec()
    } else {
        // Repeat research-heavy roles for larger swarms.
        DEFAULT_ROLES.iter().cycle().take(n).map(|s| *s).collect()
    }
}

pub fn cmd_swarm(args: &[String], data_dir: &PathBuf) {
    if args.len() < 3 {
        eprintln!("usage: pantheon swarm <N> \"<task>\" [roles...] [--delivery telegram]");
        eprintln!("       pantheon swarm status [<swarm_id>]");
        eprintln!("       pantheon swarm list");
        std::process::exit(2);
    }

    match args[2].as_str() {
        "status" | "list" => {
            handle_swarm_query(&args[2..], data_dir);
        }
        n_str => {
            // Parse N
            let n: usize = n_str.parse().unwrap_or_else(|_| {
                eprintln!("error: expected a number, got {n_str}");
                std::process::exit(2);
            });

            if n == 0 {
                eprintln!("error: swarm needs at least 1 agent");
                std::process::exit(2);
            }

            if n > 20 {
                eprintln!("error: max 20 agents per swarm (caps at defaults)");
                std::process::exit(2);
            }

            // Parse task (quoted or unquoted remainder until --delivery)
            let rest = &args[3..];
            let mut task_parts: Vec<String> = Vec::new();
            let mut roles: Vec<String> = Vec::new();
            let mut delivery: Option<String> = None;
            let mut in_roles = false;
            let mut i = 0;

            while i < rest.len() {
                match rest[i].as_str() {
                    "--delivery" => {
                        i += 1;
                        if i < rest.len() {
                            delivery = Some(rest[i].clone());
                        }
                    }
                    "--roles" => {
                        in_roles = true;
                    }
                    s if s.starts_with("--") => {
                        eprintln!("unknown flag: {s}");
                        std::process::exit(2);
                    }
                    _ => {
                        if in_roles {
                            roles.push(rest[i].clone());
                        } else {
                            task_parts.push(rest[i].clone());
                        }
                    }
                }
                i += 1;
            }

            let task = task_parts.join(" ");
            if task.is_empty() {
                eprintln!("error: need a task for the swarm");
                std::process::exit(2);
            }

            spawn_swarm(n, &task, roles, delivery, data_dir);
        }
    }
}

fn spawn_swarm(
    n: usize,
    task: &str,
    explicit_roles: Vec<String>,
    delivery: Option<String>,
    data_dir: &PathBuf,
) {
    let sup = match Supervisor::open(data_dir.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open runtime: {e}");
            std::process::exit(1);
        }
    };

    // Resolve roles.
    let role_names: Vec<String> = if !explicit_roles.is_empty() {
        if explicit_roles.len() != n {
            eprintln!(
                "error: {} roles for {} agents — counts must match",
                explicit_roles.len(),
                n
            );
            std::process::exit(2);
        }
        explicit_roles
    } else {
        default_roles(n).iter().map(|s| (*s).to_string()).collect()
    };

    // Build and check swarm caps.
    let caps = Caps::default();
    let swarm = Swarm::new(caps);

    // Check we can spawn N agents up front.
    for (i, role) in role_names.iter().enumerate() {
        if let Err(refusal) = swarm.check_spawn(0, &role) {
            eprintln!("cannot spawn agent {i} ({role}): {}", refusal.message());
            std::process::exit(1);
        }
    }

    let swarm_id = format!("swarm_{}", new_run_id());
    let mut handles: Vec<SwarmAgent> = Vec::new();

    for (i, role) in role_names.iter().enumerate() {
        let run_id = new_run_id();
        let _ = sup.start_run(&run_id);

        let agent_label = format!("{swarm_id}:{role}:{i}");

        sup.emit(pantheon_core::events::Event::AgentSpawned {
            run_id: run_id.clone(),
            agent: agent_label.clone(),
        })
        .ok();

        sup.emit(pantheon_core::events::Event::RunProgress {
            run_id: run_id.clone(),
            detail: format!("[swarm {swarm_id}] agent {i} ({role}) assigned: {task}"),
        })
        .ok();

        handles.push(SwarmAgent {
            label: agent_label,
            run_id,
            role: role.clone(),
        });
    }

    save_swarm(
        data_dir,
        &SwarmRecord {
            swarm_id: swarm_id.clone(),
            task: task.to_string(),
            roles: role_names.clone(),
            agents: handles,
            delivery: delivery.clone(),
            created_ms: now_ms(),
        },
    );

    println!(
        "swarm {} spawned\n  agents: {}\n  task: {}\n  roles: {}\n  status: pantheon swarm status {}",
        &swarm_id[..swarm_id.len().min(12)],
        n,
        task,
        role_names.join(", "),
        swarm_id
    );

    if let Some(d) = &delivery {
        println!("  delivery: {d}");
    }
}

fn handle_swarm_query(args: &[String], data_dir: &PathBuf) {
    let sup = match Supervisor::open(data_dir.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open runtime: {e}");
            std::process::exit(1);
        }
    };
    match args[0].as_str() {
        "list" => {
            let swarms = load_all_swarms(data_dir);
            if swarms.is_empty() {
                println!("no swarms recorded");
                return;
            }
            for s in &swarms {
                println!(
                    "{}  agents: {}  task: {}",
                    s.swarm_id,
                    s.agents.len(),
                    s.task.chars().take(80).collect::<String>()
                );
            }
        }
        "status" => {
            if args.len() < 2 {
                eprintln!("usage: pantheon swarm status <swarm_id>");
                std::process::exit(2);
            }
            let id = &args[1];
            let rec = match resolve_swarm(data_dir, id) {
                Ok(Some(r)) => Some(r),
                Ok(None) => reconstruct_from_ledger(&sup, id),
                Err(amb) => {
                    eprintln!("{amb}");
                    std::process::exit(1);
                }
            };
            let Some(rec) = rec else {
                eprintln!("unknown swarm '{id}' (see `pantheon swarm list`)");
                std::process::exit(1);
            };
            println!("SWARM {}", rec.swarm_id);
            println!("  task: {}", rec.task);
            println!("  roles: {}", rec.roles.join(", "));
            if let Some(d) = &rec.delivery {
                println!("  delivery: {d}");
            }
            println!("  agents: {}", rec.agents.len());
            for a in &rec.agents {
                let status = sup
                    .ledger_status(&a.run_id)
                    .unwrap_or(None)
                    .unwrap_or_else(|| "unknown".to_string());
                println!("    {}  {}  {}  {status}", a.label, a.run_id, a.role);
            }
        }
        _ => {
            // Guarded by cmd_swarm's status|list allow-list; fail loud
            // (not unreachable) so a new subcommand can't silently no-op.
            eprintln!("usage: pantheon swarm status [<swarm_id>]");
            eprintln!("       pantheon swarm list");
            std::process::exit(2);
        }
    }
}

/// Fallback for swarms spawned before manifests existed: scan the ledger
/// for `AgentSpawned` agents shaped `{swarm_id}:{role}:{i}`.
fn reconstruct_from_ledger(sup: &Supervisor, id: &str) -> Option<SwarmRecord> {
    let runs = sup.ledger_list_runs(2000).ok()?;
    let mut agents: Vec<SwarmAgent> = Vec::new();
    let mut task = String::new();
    for r in &runs {
        let entries = sup.replay(&r.0).ok()?;
        for e in &entries {
            match &e.event {
                pantheon_core::events::Event::AgentSpawned { agent, run_id }
                    if agent.starts_with(id) =>
                {
                    let role = agent.split(':').nth(1).unwrap_or("agent").to_string();
                    if !agents.iter().any(|a: &SwarmAgent| a.run_id == *run_id) {
                        agents.push(SwarmAgent {
                            label: agent.clone(),
                            run_id: run_id.clone(),
                            role,
                        });
                    }
                }
                pantheon_core::events::Event::RunProgress { detail, .. }
                    if task.is_empty() && detail.contains(id) =>
                {
                    // Detail looks like "[swarm <id>] agent i (role) assigned: <task>".
                    if let Some(t) = detail.split("assigned: ").nth(1) {
                        task = t.to_string();
                    }
                }
                _ => {}
            }
        }
    }
    if agents.is_empty() {
        return None;
    }
    agents.sort_by(|a, b| a.label.cmp(&b.label));
    let swarm_id = agents[0].label.split(':').next().unwrap_or(id).to_string();
    let roles = agents.iter().map(|a| a.role.clone()).collect();
    Some(SwarmRecord {
        swarm_id,
        task,
        roles,
        agents,
        delivery: None,
        created_ms: 0,
    })
}
