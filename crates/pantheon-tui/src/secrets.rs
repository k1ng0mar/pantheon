//! `pantheon secrets migrate`: move plaintext dotenv stores into the encrypted vault.
//!
//! Backup first, migrate, verify by reopening, never delete plaintext.
//! Deleting plaintext is a separate human approved step after restart.

use std::path::PathBuf;

fn flag_val(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

pub fn cmd_secrets(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") || args.len() < 3 {
        println!("usage: pantheon secrets migrate [--file FILE] [--dir DIR]");
        println!("  migrate gateway.env and logins.env into the encrypted vault");
        println!("  backup is written before migration, plaintext is left in place");
        return;
    }
    match args[2].as_str() {
        "migrate" => {
            let data_dir = flag_val(args, "--dir")
                .map(PathBuf::from)
                .unwrap_or_else(crate::terminal::data_dir);
            let files: Vec<String> = if let Some(f) = flag_val(args, "--file") {
                vec![f]
            } else {
                vec!["gateway.env".to_string(), "logins.env".to_string()]
            };
            let vault = data_dir.join("secrets.json");
            let key = data_dir.join(".secrets.key");
            for file in &files {
                if !data_dir.join(file).exists() {
                    println!("{file}: not present, skipping");
                    continue;
                }
                match pantheon_secrets::migration::migrate_dotenv_to_filevault(
                    &data_dir, file, &vault, &key,
                ) {
                    Ok(report) => {
                        println!(
                            "{}: migrated {} entries, verified {}",
                            report.file,
                            report.migrated,
                            if report.verified { "yes" } else { "no" }
                        );
                    }
                    Err(e) => {
                        eprintln!("{file}: migration failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            println!("plaintext originals left in place. delete only after a verified restart.");
        }
        other => {
            eprintln!("secrets: unknown subcommand {other}");
            eprintln!("usage: pantheon secrets migrate [--file FILE] [--dir DIR]");
            std::process::exit(2);
        }
    }
}
