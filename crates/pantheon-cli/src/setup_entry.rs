//! `pantheon setup` CLI entry: flag parsing over the wizard.
use crate::config_schema::PolicyPreset;
use crate::setup_cli::{run_setup, SetupAnswers};

fn flag(args: &[String], name: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

pub fn cmd_setup(args: &[String]) {
    let policy = flag(args, "--policy")
        .and_then(|p| PolicyPreset::from_str(&p))
        .or_else(|| {
            let bad = flag(args, "--policy");
            if let Some(b) = bad {
                eprintln!("setup: unknown policy {b:?} (reader|coder|coder_memory)");
                std::process::exit(2);
            }
            None
        });
    let packs = flag(args, "--packs").map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    });
    let plugins = flag(args, "--plugins").map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    });
    let answers = SetupAnswers {
        profile: flag(args, "--profile"),
        provider: flag(args, "--provider"),
        model: flag(args, "--model"),
        api_key_env: flag(args, "--api-key-env"),
        fallback_provider: flag(args, "--fallback-provider"),
        fallback_model: flag(args, "--fallback-model"),
        policy,
        memory_backend: flag(args, "--memory"),
        tool_packs: packs,
        plugins,
    };
    // --yes: accept defaults for anything without a flag (non-interactive).
    let assume = args.iter().any(|a| a == "--yes");
    run_setup(&crate::data_dir(), answers, assume);
}
