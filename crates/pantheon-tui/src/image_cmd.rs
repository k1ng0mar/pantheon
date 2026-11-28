//! `pantheon image` - configure and try image-generation services.
//!
//! The config file is the source of truth (`[image_providers.<id>]` + the
//! `[image]` pointer); this command exists so a user can see what is
//! configured, add a service without hand-editing TOML, and prove it works
//! before an agent ever calls the tool.

use pantheon_api::config::{Config, ImageProviderSection};
use pantheon_providers::image::{
    ImageRequest, catalog, generate_image, presets, resolve_spec,
};

const USAGE: &str = "\
pantheon image <command>

    list                     configured services, presets, and which one is active
    test --prompt <text>     run one real generation and report the file
         [--size 1024x1024] [--provider <id>] [--model <id>] [--out <path>]
    add <id> --backend <b> --base-url <url>
         [--model <id>] [--key-env <VAR>] [--key-header <Header>]
         [--label <name>] [--steps <n>] [--select]
    remove <id>              drop a service from the config
    select <id>              point [image].provider at a service

Backends and the services they fit:
    openai_images   POST {base}/images/generations      OpenAI-wire APIs, routers
    chat_image      POST {base}/chat/completions        models that emit images
                    with modalities [image,text]        (OpenRouter, Gemini)
    a1111           POST {base}/sdapi/v1/txt2img        local SD WebUI / Forge
    url_template    GET  <base_url>                     keyless services, small
                    with {prompt} {width} {height}      self-hosted servers
    fal             POST {base}/{model}                 fal.ai synchronous run

Keys live in the environment, never in the config: a row names the variable
(`--key-env FAL_KEY`) and pantheon reads it at call time.";

pub fn start(args: &[String]) {
    match args.first().map(String::as_str) {
        None | Some("list") => list(),
        Some("test") => test(&args[1..]),
        Some("add") => add(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("select") => select(&args[1..]),
        Some("--help") | Some("-h") | Some("help") => println!("{USAGE}"),
        Some(other) => {
            println!("unknown image subcommand {other:?}\n");
            println!("{USAGE}");
        }
    }
}

fn data_dir() -> std::path::PathBuf {
    // Matches every other CLI path: PANTHEON_HOME when set, else ~/.pantheon.
    crate::terminal::data_dir()
}

fn load() -> Option<(std::path::PathBuf, Config)> {
    let dir = data_dir();
    match Config::load(&dir) {
        Ok(cfg) => Some((dir, cfg)),
        Err(e) => {
            println!("could not read config: {e}");
            None
        }
    }
}

fn list() {
    let Some((_, cfg)) = load() else { return };
    let rows = catalog(&cfg);
    println!("image services ({} configured, {} preset):", rows.iter().filter(|r| r.configured).count(), rows.iter().filter(|r| !r.configured).count());
    for r in &rows {
        let mark = if r.selected { "*" } else { " " };
        let state = if r.configured { "set" } else { "preset" };
        println!("  {mark} {} [{state}] {} -> {}", r.id, r.backend, r.base);
        if !r.models.is_empty() {
            println!("      models: {}", r.models.join(", "));
        }
        if let Some(env) = &r.key_env {
            let present = std::env::var(env).map(|v| !v.trim().is_empty()).unwrap_or(false);
            println!(
                "      key: {env} ({})",
                if present { "present" } else { "not set" }
            );
        }
    }
    println!();
    println!("* = what [image].provider selects today");
    if let Some(endpoint) = cfg
        .image
        .as_ref()
        .and_then(|s| s.endpoint.as_deref())
        .filter(|e| !e.trim().is_empty())
    {
        println!("note: [image].endpoint is set inline ({endpoint}); it overrides the row's base_url");
    }
    if rows.iter().all(|r| !r.configured) {
        println!();
        println!("Nothing configured yet. Start with a keyless service:");
        println!("  pantheon image add pollinations --backend url_template \\");
        println!("      --base-url '{}' --select", presets()[0].base);
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a == name {
            return args.get(i + 1).cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn test(args: &[String]) {
    let prompt = flag(args, "--prompt")
        .or_else(|| {
            args.iter()
                .find(|a| !a.starts_with("--"))
                .cloned()
        })
        .filter(|p| !p.trim().is_empty());
    let Some(prompt) = prompt else {
        println!("test needs --prompt <text>\n\n{USAGE}");
        return;
    };
    let Some((dir, cfg)) = load() else { return };
    let req = ImageRequest {
        prompt: prompt.clone(),
        size: flag(args, "--size"),
        model: flag(args, "--model"),
        provider: flag(args, "--provider"),
    };
    // Show the target before spending anything on it.
    match resolve_spec(&cfg, &req) {
        Ok(spec) => println!(
            "-> {} ({}) model {}",
            spec.provider,
            spec.backend.as_str(),
            spec.model.clone().unwrap_or_else(|| "default".into())
        ),
        Err(e) => {
            println!("cannot resolve a service: [{}] {}", e.code, e.cause);
            println!();
            println!("Try `pantheon image list` to see what is configured.");
            return;
        }
    }
    let started = std::time::Instant::now();
    match generate_image(&cfg, &req) {
        Ok(out) => {
            let path = match flag(args, "--out") {
                Some(p) => std::path::PathBuf::from(p),
                None => {
                    let images = dir.join("images");
                    let _ = std::fs::create_dir_all(&images);
                    images.join(format!(
                        "image-{}.{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0),
                        out.extension()
                    ))
                }
            };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&path, &out.bytes) {
                Ok(()) => println!(
                    "ok {} ({} bytes, {}) in {:.1}s\n{}",
                    out.mime,
                    out.bytes.len(),
                    out.backend,
                    started.elapsed().as_secs_f32(),
                    path.display()
                ),
                Err(e) => println!("generated but could not write {}: {e}", path.display()),
            }
        }
        Err(e) => {
            println!("failed [{}] {}", e.code, e.cause);
            if !e.remediation.is_empty() {
                println!("fix: {}", e.remediation);
            }
        }
    }
}

fn add(args: &[String]) {
    let Some(id) = args.first().filter(|a| !a.starts_with("--")).cloned() else {
        println!("add needs a service id, e.g. `pantheon image add fal ...`\n\n{USAGE}");
        return;
    };
    let Some((dir, mut cfg)) = load() else { return };
    let backend = flag(args, "--backend").unwrap_or_else(|| "url_template".into());
    let base = flag(args, "--base-url").unwrap_or_default();
    if base.trim().is_empty() {
        println!("add needs --base-url (a service root, or a full URL with {{prompt}})");
        return;
    }
    let row = ImageProviderSection {
        label: flag(args, "--label"),
        backend: backend.clone(),
        base_url: base.clone(),
        key_env: flag(args, "--key-env"),
        key_header: flag(args, "--key-header"),
        models: flag(args, "--model")
            .map(|m| {
                vec![pantheon_api::config::CustomModel {
                    id: m,
                    ..Default::default()
                }]
            })
            .unwrap_or_default(),
        steps: flag(args, "--steps").and_then(|s| s.trim().parse().ok()),
        timeout_secs: None,
    };
    cfg.image_providers.insert(id.clone(), row);
    if args.iter().any(|a| a == "--select") {
        let mut section = cfg.image.clone().unwrap_or_default();
        section.provider = Some(id.clone());
        section.backend = None;
        section.endpoint = None;
        cfg.image = Some(section);
    }
    match cfg.save(&dir) {
        Ok(()) => println!("saved image service '{id}' ({backend}) in {}", dir.join("config.toml").display()),
        Err(e) => println!("could not save config: {e}"),
    }
}

fn remove(args: &[String]) {
    let Some(id) = args.first().cloned() else {
        println!("remove needs a service id");
        return;
    };
    let Some((dir, mut cfg)) = load() else { return };
    if cfg.image_providers.remove(&id).is_none() {
        println!("no configured image service '{id}' (presets are not config entries)");
        return;
    }
    if cfg
        .image
        .as_ref()
        .and_then(|s| s.provider.as_deref())
        .map(|p| p == id)
        .unwrap_or(false)
    {
        if let Some(section) = cfg.image.as_mut() {
            section.provider = None;
        }
        println!("note: [image].provider pointed at '{id}'; cleared it");
    }
    match cfg.save(&dir) {
        Ok(()) => println!("removed image service '{id}'"),
        Err(e) => println!("could not save config: {e}"),
    }
}

fn select(args: &[String]) {
    let Some(id) = args.first().cloned() else {
        println!("select needs a service id");
        return;
    };
    let Some((dir, mut cfg)) = load() else { return };
    if !cfg.image_providers.contains_key(&id) && !presets().iter().any(|p| p.id == id) {
        println!("unknown service '{id}'; run `pantheon image list`");
        return;
    }
    if !cfg.image_providers.contains_key(&id) {
        println!(
            "'{id}' is a preset, not a configured row. Add it first:\n  pantheon image add {id} --backend <b> --base-url <url> --select"
        );
        return;
    }
    let mut section = cfg.image.clone().unwrap_or_default();
    section.provider = Some(id.clone());
    section.backend = None;
    section.endpoint = None;
    cfg.image = Some(section);
    match cfg.save(&dir) {
        Ok(()) => println!("[image].provider = \"{id}\""),
        Err(e) => println!("could not save config: {e}"),
    }
}
