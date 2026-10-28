//! `pantheon codebase`: inspect the native symbol/call graph.
//!
//! The engine lives in `pantheon-exec::codegraph` and is also exposed to
//! the agent as `codebase.*` tools. This verb is the operator view: build
//! or refresh the cache, then search symbols or walk lexical calls.

use crate::terminal::data_dir;
use pantheon_exec::codegraph::{CodeGraph, Overview};
use std::path::{Path, PathBuf};

pub fn cmd_codebase(args: &[String]) {
    let verb = args.first().map(|s| s.as_str()).unwrap_or("overview");
    if matches!(verb, "help" | "--help" | "-h") {
        help();
        return;
    }
    let dd = data_dir();
    match verb {
        "index" => {
            let (root, json) = path_arg(args, 1);
            run_index(&root, &dd, json);
        }
        "search" => {
            let Some(query) = positional(args, 1) else {
                eprintln!("codebase search: needs a query");
                std::process::exit(2);
            };
            let (root, json) = path_arg(args, 2);
            let graph = run_load(&root, &dd);
            let nodes = graph.search(&query, 100);
            if json {
                print_json(&nodes);
            } else {
                for n in nodes {
                    println!("{:<10} {:<48} {}:{}", n.kind, n.qualified, n.file, n.line);
                }
            }
        }
        "callers" => {
            let Some(symbol) = positional(args, 1) else {
                eprintln!("codebase callers: needs a symbol");
                std::process::exit(2);
            };
            let (root, json) = path_arg(args, 2);
            let graph = run_load(&root, &dd);
            let hits = graph.callers(&symbol, 100);
            if json {
                print_json(&hits);
            } else {
                for hit in hits {
                    println!(
                        "{} at {}:{} -> {}",
                        hit.node.qualified, hit.edge.file, hit.edge.line, hit.edge.to_name
                    );
                }
            }
        }
        "callees" => {
            let Some(symbol) = positional(args, 1) else {
                eprintln!("codebase callees: needs a symbol");
                std::process::exit(2);
            };
            let (root, json) = path_arg(args, 2);
            let graph = run_load(&root, &dd);
            let edges = graph.callees(&symbol, 100);
            if json {
                print_json(&edges);
            } else {
                for edge in edges {
                    println!(
                        "{}:{} {} -> {}",
                        edge.file, edge.line, edge.from_name, edge.to_name
                    );
                }
            }
        }
        "overview" | "" => {
            let (root, json) = path_arg(args, 1);
            let graph = run_load(&root, &dd);
            print_overview(&graph.overview(), json);
        }
        other => {
            eprintln!("codebase: unknown verb '{other}'");
            eprintln!(
                "  usage: pantheon workspace codebase <index|search|callers|callees|overview> [args]"
            );
            std::process::exit(2);
        }
    }
}

fn help() {
    println!(concat!(
        "pantheon workspace codebase <command> [args]\n\n",
        "Commands:\n",
        "  index [PATH] [--json]       build/refresh the native symbol graph\n",
        "  search <query> [PATH]       search symbol names and paths\n",
        "  callers <symbol> [PATH]     inbound lexical call edges\n",
        "  callees <symbol> [PATH]     outbound lexical call edges\n",
        "  overview [PATH] [--json]    file/symbol/call counts and top dirs"
    ));
}

fn positional(args: &[String], index: usize) -> Option<String> {
    args.get(index).filter(|a| !a.starts_with('-')).cloned()
}

/// Accept an optional positional root and `--json` in any order after the
/// required argument. The path is always the first non-flag argument.
/// Anything else is an error: the old `_ => {}` silently swallowed
/// unknown flags and extra positionals, so `codebase search foo
/// --bogus` searched as if the flag had never been typed.
fn path_arg(args: &[String], start: usize) -> (PathBuf, bool) {
    let mut root: Option<PathBuf> = None;
    let mut json = false;
    for arg in args.iter().skip(start) {
        match arg.as_str() {
            "--json" => json = true,
            s if s.starts_with('-') => {
                eprintln!("codebase: unknown flag '{s}' (want --json)");
                std::process::exit(2);
            }
            s if root.is_none() => {
                root = Some(PathBuf::from(s));
            }
            s => {
                eprintln!("codebase: unexpected extra argument '{s}'");
                std::process::exit(2);
            }
        }
    }
    (
        root.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        json,
    )
}

fn run_index(root: &Path, dd: &Path, json: bool) {
    match CodeGraph::build(root) {
        Ok(graph) => {
            let cache = CodeGraph::cache_path_for(dd, root);
            if let Err(e) = graph.save(&cache) {
                eprintln!("codebase: index built but cache write failed: {e}");
            }
            print_overview(&graph.overview(), json);
        }
        Err(e) => {
            eprintln!("codebase: index failed: {e}");
            std::process::exit(1);
        }
    }
}

fn run_load(root: &Path, dd: &Path) -> CodeGraph {
    let cache = CodeGraph::cache_path_for(dd, root);
    if let Some(graph) = CodeGraph::load(&cache) {
        if graph.is_current_for(root) {
            return graph;
        }
    }
    match CodeGraph::build(root) {
        Ok(graph) => {
            let _ = graph.save(&cache);
            graph
        }
        Err(e) => {
            eprintln!("codebase: index failed: {e}");
            std::process::exit(1);
        }
    }
}

fn print_json<T: serde::Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("codebase: encode failed: {e}"),
    }
}

fn print_overview(o: &Overview, json: bool) {
    if json {
        print_json(o);
        return;
    }
    println!("root         {}", o.root);
    println!("files        {} ({} parse errors)", o.files, o.parse_errors);
    println!("symbols      {}", o.symbols);
    println!("call edges   {}", o.calls);
    println!("by kind");
    for (kind, count) in &o.by_kind {
        println!("  {kind:<12} {count}");
    }
    if !o.top_directories.is_empty() {
        println!("top directories");
        for (dir, count) in &o.top_directories {
            println!("  {dir:<48} {count}");
        }
    }
}
