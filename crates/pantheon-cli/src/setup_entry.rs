//! `pantheon setup` CLI entry: flag parsing over the wizard.
use crate::cli_args::Args;
use crate::config_schema::PolicyPreset;
use crate::setup_cli::{run_setup, SetupAnswers};

pub fn cmd_setup(args: &[String]) {
    let parsed = Args::parse(&args[1.min(args.len())..]);
    let flag = |name: &str| parsed.flag(name);
    let policy = flag("policy")
        .and_then(|p| PolicyPreset::from_str(&p))
        .or_else(|| {
            let bad = flag("policy");
            if let Some(b) = bad {
                eprintln!("setup: unknown policy {b:?} (reader|coder|coder_memory)");
                std::process::exit(2);
            }
            None
        });
    let packs = flag("packs").map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    });
    let plugins = flag("plugins").map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    });
    let answers = SetupAnswers {
        profile: flag("profile"),
        provider: flag("provider"),
        model: flag("model"),
        api_key_env: flag("api-key-env"),
        fallback_provider: flag("fallback-provider"),
        fallback_model: flag("fallback-model"),
        policy,
        memory_backend: flag("memory"),
        tool_packs: packs,
        plugins,
        key: flag("key"),
    };
    // --yes: accept defaults for anything without a flag (non-interactive).
    run_setup(&crate::data_dir(), answers, parsed.has("yes"));
}
