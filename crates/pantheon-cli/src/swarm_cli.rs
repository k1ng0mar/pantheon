//! `pantheon swarm` — coordinate multiple agent runs toward a shared objective.
//!
//! Architecture (§3): a swarm creates N independent Runs, each with an
//! optional role assignment. The runtime enforces concurrency, token, and
//! cost caps via pantheon_swarm::Caps. A final synthesis Run merges findings.

use pantheon_runtime::{new_run_id, Supervisor};
use pantheon_swarm::{Caps, Swarm};
use std::path::PathBuf;

/// Default agent roles assigned by count. The user can override with explicit
/// roles: pantheon swarm 5 "task" researcher critic verifier analyst synthesizer
const DEFAULT_ROLES: &[&str] = &[
    "researcher", "researcher", "verifier", "critic", "synthesizer",
];

fn default_roles(n: usize) -> Vec<&'static str> {
    if n <= DEFAULT_ROLES.len() {
        DEFAULT_ROLES[..n].to_vec()
    } else {
        // Repeat research-heavy roles for larger swarms.
        DEFAULT_ROLES
            .iter()
            .cycle()
            .take(n)
            .map(|s| *s)
            .collect()
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
    let mut handles = Vec::new();

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
            detail: format!(
                "[swarm {swarm_id}] agent {i} ({role}) assigned: {task}"
            ),
        })
        .ok();

        handles.push((agent_label, run_id, role.clone()));
    }

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

fn handle_swarm_query(args: &[String], _data_dir: &PathBuf) {
    match args[0].as_str() {
        "list" => {
            // TODO: load from storage once swarm state is persisted
            println!("swarm list — not yet persisted");
        }
        "status" => {
            if args.len() < 2 {
                eprintln!("usage: pantheon swarm status <swarm_id>");
                std::process::exit(2);
            }
            let id = &args[1];
            println!("SWARM {}", &id[..id.len().min(12)]);
            println!("  (status tracking coming — swarm state isn't persisted yet)");
        }
        _ => unreachable!(),
    }
}
