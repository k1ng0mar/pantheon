//! `pantheon migrate`: the section 23 pipeline, exposed.
//!
//!   detect    — which sources are on this machine, and where
//!   plan      — dry run: what would import, what archives, what is a secret
//!   apply     — backup, then import, then validate (the only writer)
//!   validate  — re-check the current targets without writing
//!
//! The default for every verb is read-only. `apply` is the single verb that
//! touches the data dir, and it refuses to run without either an explicit
//! `--yes` or a typed confirmation, because it overwrites existing targets.
//!
//! Secrets are never a candidate for import. A `.env`, `auth.json`,
//! `models.yml`, `agent.db`, `broker.token`, or any `*.pem` / `*.key` is
//! reported and skipped — archiving a credential would turn this verb into
//! an exfil path, so it does not.

use crate::data_dir;
use pantheon_migrate::{
    analyze, apply_with, backup, detect, index_quarantine, plan, quarantine_dir, reconcile_keys,
    render, validate, ItemKind, KeyMatch, MigrationPlan, SourceKind, Targets,
};
use std::path::{Path, PathBuf};

/// Where each source lives by default, in `detect` order.
fn default_roots() -> Vec<(SourceKind, PathBuf)> {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    vec![
        (SourceKind::Hermes, home.join(".hermes")),
        (SourceKind::OpenClaw, home.join(".openclaw")),
        (SourceKind::Omp, home.join(".omp")),
    ]
}

fn ext_dir() -> PathBuf {
    std::env::var("PANTHEON_EXT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir().join("extensions"))
}

fn targets() -> Targets {
    Targets::new(data_dir(), ext_dir())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Resolve the source root: an explicit `--path`, else the conventional home.
fn resolve_root(kind: SourceKind, args: &[String]) -> PathBuf {
    if let Some(p) = flag_value(args, "--path") {
        return PathBuf::from(p);
    }
    if let Some(p) = args.iter().rev().find(|a| !a.starts_with('-')) {
        // A bare trailing path is accepted, so `migrate plan hermes ~/.hermes`
        // works without the flag.
        let cand = PathBuf::from(p);
        if cand.is_dir() {
            return cand;
        }
    }
    default_roots()
        .into_iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, p)| p)
        .unwrap_or_default()
}

/// `--kind skill,agent` — restrict the plan to these item kinds.
fn kind_filter(args: &[String]) -> Option<Vec<ItemKind>> {
    let raw = flag_value(args, "--kind")?;
    let mut out = Vec::new();
    for part in raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let found = [
            ItemKind::Skill,
            ItemKind::Agent,
            ItemKind::Rule,
            ItemKind::Command,
            ItemKind::Prompt,
            ItemKind::Extension,
            ItemKind::Persona,
            ItemKind::Memory,
            ItemKind::Provider,
            ItemKind::Mcp,
            ItemKind::Schedule,
            ItemKind::Channel,
            ItemKind::Session,
            ItemKind::Credentials,
            ItemKind::Provider,
            ItemKind::Opaque,
        ]
        .into_iter()
        .find(|k| k.name() == part);
        match found {
            Some(k) => out.push(k),
            None => {
                eprintln!("migrate: unknown kind '{part}'");
                std::process::exit(2);
            }
        }
    }
    Some(out)
}

fn filtered(plan: &MigrationPlan, kinds: &Option<Vec<ItemKind>>) -> MigrationPlan {
    let Some(kinds) = kinds else {
        return plan.clone();
    };
    MigrationPlan {
        source: plan.source.clone(),
        root: plan.root.clone(),
        source_version: plan.source_version.clone(),
        items: plan
            .items
            .iter()
            .filter(|i| kinds.contains(&i.kind))
            .cloned()
            .collect(),
    }
}

fn print_json<T: serde::Serialize>(v: &T) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("migrate: encode failed: {e}"),
    }
}

/// `pantheon migrate detect [path]`
pub fn cmd_migrate_detect(args: &[String]) {
    let roots: Vec<PathBuf> = match flag_value(args, "--path") {
        Some(p) => vec![PathBuf::from(p)],
        None => default_roots().into_iter().map(|(_, p)| p).collect(),
    };
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut any = false;
    for root in &roots {
        if !root.is_dir() {
            continue;
        }
        let kinds = detect(root);
        if kinds.is_empty() {
            continue;
        }
        any = true;
        for k in kinds {
            let version = pantheon_migrate::source_version(root, k);
            rows.push(serde_json::json!({
                "source": k.name(),
                "root": root.to_string_lossy(),
                "version": version,
                "items": analyze(root, k).len(),
            }));
        }
    }
    if has_flag(args, "--json") {
        print_json(&rows);
        return;
    }
    if !any {
        println!("no known source detected under {}", roots[0].display());
        return;
    }
    println!("{:<10} {:<7} {}", "SOURCE", "ITEMS", "ROOT");
    for r in &rows {
        println!(
            "{:<10} {:<7} {}",
            r["source"].as_str().unwrap_or("?"),
            r["items"].as_i64().unwrap_or(0),
            r["root"].as_str().unwrap_or("?")
        );
    }
}

/// `pantheon migrate plan <source> [path] [--kind K] [--json]`
pub fn cmd_migrate_plan(args: &[String]) {
    let Some(kind) = source_arg(args) else {
        return;
    };
    let root = resolve_root(kind, args);
    if !root.is_dir() {
        eprintln!("migrate: {} root not found: {}", kind, root.display());
        std::process::exit(1);
    }
    let t = targets();
    let full = plan(&root, kind, &t);
    let selected = filtered(&full, &kind_filter(args));
    if has_flag(args, "--json") {
        print_json(&selected);
    } else {
        print!("{}", render(&selected));
        println!("  target root: {}", t.data_dir.display());
        println!("  nothing written; re-run with `--apply` to import");
    }
}

/// `pantheon migrate apply <source> [path] [--kind K] [--yes] [--json]`
///
/// The approval gate. `--yes` skips the prompt; otherwise the operator has
/// to type the source name, because this overwrites existing targets.
pub fn cmd_migrate_apply(args: &[String]) {
    let Some(kind) = source_arg(args) else {
        return;
    };
    let root = resolve_root(kind, args);
    if !root.is_dir() {
        eprintln!("migrate: {} root not found: {}", kind, root.display());
        std::process::exit(1);
    }
    let t = targets();
    let full = plan(&root, kind, &t);
    let selected = filtered(&full, &kind_filter(args));

    if selected.imports() == 0 {
        println!("nothing to import from {}", kind);
        if selected.skipped() > 0 {
            println!(
                "  {} credential path(s) skipped by policy",
                selected.skipped()
            );
        }
        return;
    }

    if !has_flag(args, "--yes") {
        print!("{}", render(&selected));
        println!("about to import {} item(s):", selected.imports());
        for i in &selected.items {
            if let Some(target) = i.target() {
                println!("  {:<10} {}", i.kind, target);
            }
        }
        if selected.archived() > 0 || selected.skipped() > 0 {
            println!(
                "  ({} archived, {} credential paths skipped)",
                selected.archived(),
                selected.skipped()
            );
        }
        println!(
            "existing targets are backed up first, under {}",
            targets().backup_root().display()
        );
        print!("type '{}' to confirm: ", kind);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
        if buf.trim() != kind.name() {
            println!("aborted");
            return;
        }
    }

    // backup -> apply -> validate, in that order, each reported.
    let manifest = match backup(&selected, &t) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("migrate: backup failed: {e}");
            std::process::exit(1);
        }
    };
    if !manifest.is_empty() {
        println!(
            "backup: {} pre-image(s) under {}",
            manifest.len(),
            t.backup_root().join(&manifest.id).display()
        );
    }

    let merge_providers = has_flag(args, "--merge-providers");
    let report = match apply_with(&selected, &t, &manifest, merge_providers) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("migrate: apply failed: {e}");
            std::process::exit(1);
        }
    };

    let v = validate(&selected);
    if has_flag(args, "--json") {
        print_json(&serde_json::json!({
            "backup": manifest,
            "apply": report,
            "validate": v,
        }));
    } else {
        for o in &report.outcomes {
            let mark = match o.status {
                pantheon_migrate::ApplyStatus::Created => "+",
                pantheon_migrate::ApplyStatus::Replaced => "~",
                pantheon_migrate::ApplyStatus::Unchanged => "=",
                pantheon_migrate::ApplyStatus::MemoryPlane => ">",
                _ => "!",
            };
            println!("  {} {:<10} {}", mark, o.kind, o.target);
            if !o.detail.is_empty() {
                println!("      {}", o.detail);
            }
            for s in &o.skipped {
                println!("      skipped: {s}");
            }
        }
        print!("{}", v.render());
    }

    if !report.complete || !v.complete {
        eprintln!(
            "migrate: import incomplete — {} failure(s), {} validation problem(s)",
            report.failures(),
            v.problems()
        );
        eprintln!(
            "  restore with: cp -a {}/. {}",
            t.backup_root().join(&manifest.id).display(),
            t.data_dir.display()
        );
        std::process::exit(1);
    }
    println!(
        "migrated {} from {} — {} imported, {} archived, {} credentials skipped",
        kind,
        root.display(),
        report.ok(),
        selected.archived(),
        selected.skipped()
    );
    print_key_reconciliation(&t);
    index_imported_sessions(&t, &root, kind);
}

/// Make a migrated history actually searchable.
///
/// The quarantine copy is otherwise a file nobody reads. This indexes it into
/// the same `session_search` store the live runtime writes, under a
/// `migrated:<source>:<session>` run id so a hit can never be mistaken for a
/// real ledger event. Idempotent: `chunk_id` is derived from
/// (source, file, line), so re-running converges instead of duplicating.
///
/// Best-effort by design. A failure here means the transcripts are on disk but
/// not yet findable, which is not a reason to fail an otherwise complete
/// migration — so it is reported and the run still exits 0.
fn index_imported_sessions(t: &Targets, root: &Path, kind: SourceKind) {
    let q = quarantine_dir(&t.data_dir, kind.name());
    if !q.is_dir() {
        return;
    }
    let index_path = t.data_dir.join("ledger.db");
    let index = match pantheon_storage::search::SessionSearch::open(&index_path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("migrate: session indexing skipped: {e}");
            return;
        }
    };
    match index_quarantine(&q, kind.name(), &index) {
        Ok(r) if r.chunks > 0 => {
            println!();
            println!(
                "indexed {} transcript file(s), {} chunk(s) into session_search (run_id prefix 'migrated:{}:')",
                r.files,
                r.chunks,
                kind.name()
            );
            if !r.empty_files.is_empty() {
                println!(
                    "  {} file(s) produced nothing and were named in the manifest",
                    r.empty_files.len()
                );
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("migrate: session indexing failed: {e}"),
    }
    let _ = root;
}

/// Which carried keys pantheon's catalog will actually pick up.
///
/// The credential carry moves any provider/channel/mcp key whose name looks
/// right, and pantheon's catalog names each provider's `key_env` after the
/// same `<PROVIDER>_API_KEY` convention. That alignment usually works, but
/// nothing verified it — so a key that matches no catalog entry used to look
/// identical to one that does. This is that check.
fn print_key_reconciliation(t: &Targets) {
    let Ok(text) = std::fs::read_to_string(pantheon_migrate::pantheon_env_path(&t.data_dir)) else {
        return;
    };
    let names: Vec<String> = pantheon_migrate::parse_env_names(&text)
        .into_iter()
        .map(|e| e.name)
        .collect();
    if names.is_empty() {
        return;
    }
    let reports = reconcile_keys(&names);
    let catalogued: Vec<&pantheon_migrate::KeyReport> = reports
        .iter()
        .filter(|r| r.match_kind == KeyMatch::Catalogued)
        .collect();
    let rename: Vec<&pantheon_migrate::KeyReport> = reports
        .iter()
        .filter(|r| matches!(r.match_kind, KeyMatch::NeedsRename { .. }))
        .collect();
    if catalogued.is_empty() && rename.is_empty() {
        return;
    }
    println!();
    println!("key store vs pantheon's provider catalog:");
    if !catalogued.is_empty() {
        println!(
            "  {} key(s) already match a catalog provider and are usable as-is:",
            catalogued.len()
        );
        for r in &catalogued {
            println!(
                "    {} -> provider {}",
                r.env_var,
                r.provider.as_deref().unwrap_or("?")
            );
        }
    }
    for r in &rename {
        if let KeyMatch::NeedsRename { catalog_env } = &r.match_kind {
            println!(
                "    {} matches provider {} but pantheon reads {} — rename it to be picked up",
                r.env_var,
                r.provider.as_deref().unwrap_or("?"),
                catalog_env
            );
        }
    }
}

/// `pantheon migrate validate <source> [path] [--kind K] [--json]`
pub fn cmd_migrate_validate(args: &[String]) {
    let Some(kind) = source_arg(args) else {
        return;
    };
    let root = resolve_root(kind, args);
    let t = targets();
    let full = plan(&root, kind, &t);
    let selected = filtered(&full, &kind_filter(args));
    let v = validate(&selected);
    if has_flag(args, "--json") {
        print_json(&v);
    } else {
        print!("{}", v.render());
    }
    if !v.complete {
        std::process::exit(1);
    }
}

/// `pantheon migrate show <source> [path]` — the detected items with their
/// disposition, without building import targets. Read-only, no plan.
pub fn cmd_migrate_show(args: &[String]) {
    let Some(kind) = source_arg(args) else {
        return;
    };
    let root = resolve_root(kind, args);
    if !root.is_dir() {
        eprintln!("migrate: {} root not found: {}", kind, root.display());
        std::process::exit(1);
    }
    let detected = analyze(&root, kind);
    if has_flag(args, "--json") {
        print_json(&detected);
        return;
    }
    println!("{:<12} {:<5} {}", "KIND", "MAP", "PATH");
    for d in &detected {
        println!(
            "{:<12} {:<5} {}",
            d.kind,
            if d.mappable { "yes" } else { "no" },
            d.path
        );
        println!("             {}", d.note);
    }
    println!(
        "\n{} detected, {} mappable, {} credential path(s)",
        detected.len(),
        detected.iter().filter(|d| d.mappable).count(),
        detected
            .iter()
            .filter(|d| d.kind == ItemKind::Secret)
            .count()
    );
}

/// The first non-flag argument is the source name, unless it is a verb.
fn source_arg(args: &[String]) -> Option<SourceKind> {
    for a in args {
        if a.starts_with('-') {
            continue;
        }
        if matches!(
            a.as_str(),
            "detect" | "plan" | "apply" | "validate" | "show"
        ) {
            continue;
        }
        match SourceKind::parse(a) {
            Some(k) => return Some(k),
            None => {
                eprintln!("migrate: unknown source '{a}'");
                eprintln!("  known: hermes, openclaw, omp (aliases: oh-my-pi, pi)");
                std::process::exit(2);
            }
        }
    }
    eprintln!("migrate: name a source");
    eprintln!("  usage: pantheon migrate <detect|show|plan|apply|validate> <hermes|openclaw|omp>");
    std::process::exit(2);
}

/// Dispatch. `verb` is args[2] from main's match arm.
pub fn dispatch(verb: &str, args: &[String]) {
    match verb {
        "detect" => cmd_migrate_detect(args),
        "show" => cmd_migrate_show(args),
        "plan" => cmd_migrate_plan(args),
        "apply" => cmd_migrate_apply(args),
        "validate" => cmd_migrate_validate(args),
        other => {
            eprintln!("migrate: unknown verb '{other}'");
            eprintln!(
                "  usage: pantheon migrate <detect|show|plan|apply|validate> <source> [path]"
            );
            std::process::exit(2);
        }
    }
}

/// `migrate` needs a verb before anything else, so this is the entry main
/// calls with the full arg tail.
pub fn cmd_migrate(args: &[String]) {
    let Some(verb) = args.first() else {
        eprintln!("usage: pantheon migrate <detect|show|plan|apply|validate> <hermes|openclaw|omp> [path]");
        eprintln!("  detect    which sources are installed here");
        eprintln!("  show      every detected item and its disposition");
        eprintln!("  plan      dry run: import / archive / skip (read-only)");
        eprintln!("  apply     backup, import, validate (the only writer)");
        eprintln!("  validate  re-check current targets, no writes");
        eprintln!();
        eprintln!("  item kinds for --kind:");
        eprintln!("    skill, agent, rule, command, prompt, extension, persona, memory");
        eprintln!("    provider, mcp, schedule, channel, session, credentials");
        eprintln!();
        eprintln!("  credentials are carried into <data_dir>/.env, pantheon's own key");
        eprintln!("  store. An existing key is never overwritten; it is reported instead.");
        eprintln!("  custom providers are written to <data_dir>/providers/imported.toml");
        eprintln!("  for review; add --merge-providers to also merge them into config.toml.");
        std::process::exit(2);
    };
    dispatch(verb, &args[1..]);
}
