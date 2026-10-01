//! `pantheon config` verbs: read and write `config.toml` from the terminal.
//!
//! - `pantheon config set <dotted.key> <value>` writes one value, creating
//!   intermediate tables as needed. Edits are format-preserving: comments and
//!   layout elsewhere in the file are left alone.
//! - `pantheon config get <dotted.key>` prints one value.
//! - `pantheon config edit` opens the file in `$VISUAL`/`$EDITOR` (falling
//!   back to `vi`, then `nano`), the way you would edit any file by hand.
//! - `pantheon config path` prints the file location.
//!
//! Values are type-inferred: `true`/`false` become booleans, `100` an
//! integer, `1.5` a float, anything else a string. Surround with quotes to
//! force a string (`'"100"'` stays the text "100"). Arrays and tables cannot
//! be set from the command line; the error tells you to use `config edit`.
//!
//! Every key is validated against the real [`pantheon_api::config::Config`]
//! schema before anything is written: a typo like `goal.max_iterationz`
//! fails loudly instead of persisting a dead key. Writes are atomic
//! (temp file + fsync + rename), and `config edit` validates the edited
//! text before replacing the file, keeping a `config.toml.bak` backup.

use std::path::{Path, PathBuf};
use std::process::Command;
use toml_edit::{DocumentMut, Item, Value};

pub fn config_path() -> PathBuf {
    crate::terminal::data_dir().join("config.toml")
}

fn read_doc_at(path: &Path) -> Result<DocumentMut, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<DocumentMut>()
            .map_err(|e| format!("config: cannot parse {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(format!("config: cannot read {}: {e}", path.display())),
    }
}

/// Unique temp path in the same directory as `path`, so the eventual rename
/// is atomic. Includes pid + nanos so concurrent writers never collide.
fn tmp_path_for(path: &Path, tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let file_name = format!(
        "{}.{tag}-{}-{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config".to_string()),
        std::process::id(),
        nanos
    );
    path.with_file_name(file_name)
}

/// fsync `src`, then atomically rename it over `dst`, then best-effort
/// fsync the parent directory so the rename survives a crash.
fn fsync_and_rename(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::File::open(src)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("config: cannot fsync {}: {e}", src.display()))?;
    std::fs::rename(src, dst)
        .map_err(|e| format!("config: cannot replace {}: {e}", dst.display()))?;
    if let Some(parent) = dst.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Atomically replace `path` with `doc`: write to a temp file in the same
/// directory, fsync, rename. A crash mid-write leaves either the old file
/// or the new one, never a torn file. Temp leftovers are cleaned up on
/// every error path.
fn write_doc_atomic_at(path: &Path, doc: &DocumentMut) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("config: cannot create {}: {e}", parent.display()))?;
    }
    let tmp = tmp_path_for(path, "write");
    let failed = |e: String| {
        let _ = std::fs::remove_file(&tmp);
        e
    };
    if let Err(e) = std::fs::write(&tmp, doc.to_string()) {
        return Err(failed(format!(
            "config: cannot write {}: {e}",
            tmp.display()
        )));
    }
    fsync_and_rename(&tmp, path).map_err(failed)
}

/// Split `a.b.c` into key segments, rejecting empty or malformed segments.
fn split_key(key: &str) -> Result<Vec<&str>, String> {
    if key.is_empty() {
        return Err("config: empty key".to_string());
    }
    let parts: Vec<&str> = key.split('.').collect();
    for p in &parts {
        if p.is_empty() {
            return Err(format!("config: bad key {key:?}: empty segment"));
        }
        if !p
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "config: bad key {key:?}: segment {p:?} is not a bare key"
            ));
        }
    }
    Ok(parts)
}

/// `key=value` single-argument form, or `None` when there is no `=`.
fn split_key_eq(arg: &str) -> Option<(&str, &str)> {
    let (k, v) = arg.split_once('=')?;
    if k.is_empty() {
        return None;
    }
    Some((k, v))
}

/// Infer a TOML value from shell text. Quoted text is always a string.
/// Arrays and tables are rejected: `config set` handles scalars only.
fn infer_value(raw: &str) -> Result<Value, String> {
    let t = raw.trim();
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"')) || (t.starts_with('\'') && t.ends_with('\'')))
    {
        return Ok(Value::from(&t[1..t.len() - 1]));
    }
    if t.starts_with('[') || t.starts_with('{') {
        return Err(
            "config: arrays and tables cannot be set with `config set`; use `pantheon config edit` instead"
                .to_string(),
        );
    }
    match t {
        "true" => return Ok(Value::from(true)),
        "false" => return Ok(Value::from(false)),
        _ => {}
    }
    if let Ok(n) = t.parse::<i64>() {
        return Ok(Value::from(n));
    }
    if let Ok(n) = t.parse::<f64>() {
        return Ok(Value::from(n));
    }
    Ok(Value::from(t))
}

/// Check that `key` names a real field of the [`pantheon_api::config::Config`]
/// schema, without hardcoding the field list.
///
/// The edited document is round-tripped through the `Config` type: TOML
/// text -> `toml::Value` -> `Config` -> `toml::Value`. Serde silently drops
/// unknown keys on deserialize, so if a path segment present in the input
/// is missing from the output, that segment is not a real field. Type
/// mismatches (e.g. a string where the schema wants an integer) fail the
/// deserialization itself, so they fail loudly too.
fn validate_key_against_schema(doc: &DocumentMut, key: &str, parts: &[&str]) -> Result<(), String> {
    let input: toml::Value = doc
        .to_string()
        .parse()
        .map_err(|e| format!("config: cannot parse edited config as TOML: {e}"))?;
    let cfg: pantheon_api::config::Config = input
        .clone()
        .try_into()
        .map_err(|e| format!("config: cannot set {key:?}: {e}"))?;
    let output = toml::Value::try_from(&cfg)
        .map_err(|e| format!("config: internal error re-serializing config: {e}"))?;
    let mut in_node = &input;
    let mut out_node = &output;
    for (i, part) in parts.iter().enumerate() {
        match (in_node.get(part), out_node.get(part)) {
            (Some(next_in), Some(next_out)) => {
                in_node = next_in;
                out_node = next_out;
            }
            (Some(_), None) => {
                let parent = parts[..i].join(".");
                let location = if parent.is_empty() {
                    "the top level of the config".to_string()
                } else {
                    format!("{parent:?}")
                };
                return Err(format!(
                    "config: unknown key {key:?}: segment {part:?} is not a field of {location}"
                ));
            }
            (None, _) => {
                return Err(format!("config: internal error validating {key:?}"));
            }
        }
    }
    Ok(())
}

/// Parse + deserialize + validate candidate config text. Used by `edit`
/// before the edited file is allowed to replace the real one.
fn validate_config_text(text: &str) -> Result<pantheon_api::config::Config, String> {
    let cfg: pantheon_api::config::Config = toml::from_str(text)
        .map_err(|e| format!("config: edited file is not valid config: {e}"))?;
    let problems = cfg.validate();
    if !problems.is_empty() {
        return Err(format!(
            "config: edited file failed validation:\n  {}",
            problems.join("\n  ")
        ));
    }
    Ok(cfg)
}

fn set_dotted(doc: &mut DocumentMut, key: &str, value: Value) -> Result<(), String> {
    let parts = split_key(key)?;
    let mut table = doc.as_table_mut();
    for part in &parts[..parts.len() - 1] {
        let entry = table
            .entry(part)
            .or_insert_with(|| Item::Table(toml_edit::Table::new()));
        match entry {
            Item::Table(t) => table = t,
            other => {
                return Err(format!(
                    "config: cannot set {key:?}: {part:?} is already a {}",
                    item_kind(other)
                ));
            }
        }
    }
    table.insert(parts[parts.len() - 1], Item::Value(value));
    Ok(())
}

fn get_dotted<'d>(doc: &'d DocumentMut, key: &str) -> Result<Option<&'d Item>, String> {
    let parts = split_key(key)?;
    let mut item: &Item = doc.as_item();
    for part in &parts {
        item = match item {
            Item::Table(t) => t.get(part),
            _ => None,
        }
        .ok_or_else(|| format!("config: {key:?} is not set"))?;
    }
    Ok(Some(item))
}

fn item_kind(item: &Item) -> &'static str {
    match item {
        Item::Value(_) => "value",
        Item::Table(_) => "table",
        Item::ArrayOfTables(_) => "array of tables",
        Item::None => "nothing",
    }
}

/// Print a value the way a person would read it: strings bare, scalars
/// without their TOML decor whitespace, everything else as TOML.
fn display_value(item: &Item) -> String {
    match item {
        Item::Value(Value::String(s)) => s.value().to_string(),
        Item::Value(Value::Integer(i)) => i.value().to_string(),
        Item::Value(Value::Float(f)) => f.value().to_string(),
        Item::Value(Value::Boolean(b)) => b.value().to_string(),
        Item::Value(Value::Datetime(d)) => d.value().to_string(),
        Item::Value(Value::Array(a)) => a.to_string(),
        Item::Value(Value::InlineTable(t)) => t.to_string(),
        Item::Table(t) => t.to_string(),
        Item::ArrayOfTables(a) => a.to_string(),
        Item::None => String::new(),
    }
}

/// Set one key in the config file at `path`. Pure core of `config set`:
/// no process exits, so tests can drive it against a temp dir.
fn set_config_value(path: &Path, key: &str, raw: &str) -> Result<String, String> {
    let value = infer_value(raw)?;
    let mut doc = read_doc_at(path)?;
    let parts = split_key(key)?;
    set_dotted(&mut doc, key, value.clone())?;
    validate_key_against_schema(&doc, key, &parts)?;
    write_doc_atomic_at(path, &doc)?;
    let shown = match &value {
        Value::String(s) => format!("{:?}", s.value()),
        v => v.to_string(),
    };
    Ok(format!("set {key} = {shown} ({})", path.display()))
}

/// Read one key from the config file at `path`.
fn get_config_value(path: &Path, key: &str) -> Result<String, String> {
    let doc = read_doc_at(path)?;
    match get_dotted(&doc, key) {
        Ok(Some(item)) => Ok(display_value(item)),
        Ok(None) => Err(format!("config: {key:?} is not set")),
        Err(e) => Err(e),
    }
}

fn cmd_set(rest: &[String]) {
    let (key, raw) = match rest {
        [k, v, ..] => (k.as_str(), v.as_str()),
        [kv] => match split_key_eq(kv) {
            Some((k, v)) => (k, v),
            None => {
                eprintln!("usage: pantheon config set <dotted.key> <value>");
                std::process::exit(2);
            }
        },
        [] => {
            eprintln!("usage: pantheon config set <dotted.key> <value>");
            std::process::exit(2);
        }
    };
    match set_config_value(&config_path(), key, raw) {
        Ok(msg) => println!("{msg}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn cmd_get(rest: &[String]) {
    let key = rest.first().map(String::as_str).unwrap_or("");
    if key.is_empty() {
        eprintln!("usage: pantheon config get <dotted.key>");
        std::process::exit(2);
    }
    match get_config_value(&config_path(), key) {
        Ok(v) => println!("{v}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// The editor cascade: `$VISUAL`, then `$EDITOR`, then `vi`, then `nano`.
/// The first binary found on PATH wins; a spawn failure moves to the next.
fn pick_editor() -> Option<String> {
    for var in ["PANTHEON_EDITOR", "VISUAL", "EDITOR"] {
        if let Ok(e) = std::env::var(var) {
            let e = e.trim().to_string();
            if !e.is_empty() {
                return Some(e);
            }
        }
    }
    None
}

fn editor_exists(editor: &str) -> bool {
    let prog = editor.split_whitespace().next().unwrap_or(editor);
    // Absolute path: check directly. Otherwise search PATH.
    if prog.contains('/') {
        return std::path::Path::new(prog).is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(prog).is_file()))
        .unwrap_or(false)
}

/// Launch `editor` (a `code --wait`-style command line) on `file`.
/// Returns an error when the editor cannot be launched or exits nonzero.
fn launch_editor(editor: &str, file: &Path) -> Result<(), String> {
    let mut parts = editor.split_whitespace();
    let prog = parts.next().unwrap_or(editor);
    let status = Command::new(prog)
        .args(parts)
        .arg(file)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("config: could not launch {editor:?}: {e}"))?;
    if !status.success() {
        return Err(format!("config: editor exited with status {status}"));
    }
    Ok(())
}

/// The testable core of `config edit`: the editor works on a staging file,
/// never the live config. After the editor exits, the staged text is
/// validated; only then does it replace the real file (atomically), with
/// the pre-edit contents kept as `config.toml.bak`. A failed validation
/// deletes the stage and leaves the original untouched.
fn run_edit_flow(path: &Path, editor: &str) -> Result<String, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("config: cannot create {}: {e}", parent.display()))?;
    }
    let stage = tmp_path_for(path, "edit");
    if path.exists() {
        std::fs::copy(path, &stage)
            .map_err(|e| format!("config: cannot stage {}: {e}", path.display()))?;
    } else {
        std::fs::write(&stage, "")
            .map_err(|e| format!("config: cannot stage {}: {e}", path.display()))?;
    }
    let cleanup_stage = |e: String| {
        let _ = std::fs::remove_file(&stage);
        e
    };
    if let Err(e) = launch_editor(editor, &stage) {
        return Err(cleanup_stage(e));
    }
    let text = std::fs::read_to_string(&stage)
        .map_err(|e| cleanup_stage(format!("config: cannot read edited file: {e}")))?;
    if let Err(e) = validate_config_text(&text) {
        return Err(cleanup_stage(format!(
            "{e}\nconfig: original file left unchanged"
        )));
    }
    let backup = path.with_extension("toml.bak");
    if path.exists() {
        std::fs::copy(path, &backup).map_err(|e| {
            cleanup_stage(format!("config: cannot back up {}: {e}", path.display()))
        })?;
    }
    fsync_and_rename(&stage, path).map_err(cleanup_stage)?;
    Ok(format!(
        "saved {} (backup: {})",
        path.display(),
        backup.display()
    ))
}

fn cmd_edit() {
    let path = config_path();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(e) = pick_editor() {
        candidates.push(e);
    }
    candidates.push("vi".to_string());
    candidates.push("nano".to_string());

    for editor in &candidates {
        if !editor_exists(editor) {
            continue;
        }
        match run_edit_flow(&path, editor) {
            Ok(msg) => {
                println!("{msg}");
                return;
            }
            Err(e) if e.starts_with("config: could not launch") => continue,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
    eprintln!("config: no editor found (set $EDITOR, or install vi/nano)");
    std::process::exit(1);
}

fn config_help() {
    eprintln!("usage: pantheon config <set|get|edit|path> ...");
    eprintln!("  set <dotted.key> <value>   set one value (creates tables as needed)");
    eprintln!("  set <dotted.key>=<value>   single-argument form");
    eprintln!("  get <dotted.key>           print one value");
    eprintln!("  edit                       open config.toml in your editor");
    eprintln!("  path                       print the config file path");
    eprintln!();
    eprintln!("values: true/false -> boolean, 100 -> integer, 1.5 -> float,");
    eprintln!("anything else -> string. Quote to force a string: '\"100\"'.");
    eprintln!("arrays and tables need `pantheon config edit`.");
    eprintln!();
    eprintln!("keys are checked against the real config schema: unknown");
    eprintln!("keys are rejected instead of silently written.");
}

/// Entry point called from the verb dispatch in `terminal::run`.
pub fn cmd_config(args: &[String]) {
    match args.get(2).map(String::as_str) {
        Some("set") => cmd_set(&args[3..]),
        Some("get") => cmd_get(&args[3..]),
        Some("edit") => cmd_edit(),
        Some("path") => println!("{}", config_path().display()),
        // `--help` used to fall through to the usage-error branch (exit
        // 2); help is not an error.
        Some("--help") | Some("-h") => {
            config_help();
            std::process::exit(0);
        }
        _ => {
            config_help();
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch dir for one test. Unique per call so tests stay parallel-safe.
    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-config-verb-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scratch_config(tag: &str) -> PathBuf {
        scratch_dir(tag).join("config.toml")
    }

    #[test]
    fn infer_value_types() {
        assert!(matches!(infer_value("true"), Ok(Value::Boolean(_))));
        assert!(matches!(infer_value("false"), Ok(Value::Boolean(_))));
        assert!(matches!(infer_value("100"), Ok(Value::Integer(_))));
        assert!(matches!(infer_value("-7"), Ok(Value::Integer(_))));
        assert!(matches!(infer_value("1.5"), Ok(Value::Float(_))));
        assert!(matches!(infer_value("openai/gpt-4o"), Ok(Value::String(_))));
        // Quoted numbers stay strings.
        match infer_value("\"100\"") {
            Ok(Value::String(s)) => assert_eq!(s.value(), "100"),
            v => panic!("expected string, got {v:?}"),
        }
        match infer_value("'true'") {
            Ok(Value::String(s)) => assert_eq!(s.value(), "true"),
            v => panic!("expected string, got {v:?}"),
        }
    }

    #[test]
    fn infer_value_rejects_structured_values() {
        for raw in ["[1, 2]", "[]", "{a = 1}", "[[x]]", "[unclosed"] {
            let err = infer_value(raw).unwrap_err();
            assert!(
                err.contains("config edit"),
                "expected edit direction for {raw:?}, got: {err}"
            );
        }
        // ...but quoted structured text is a legitimate string.
        match infer_value("\"[1, 2]\"") {
            Ok(Value::String(s)) => assert_eq!(s.value(), "[1, 2]"),
            v => panic!("expected string, got {v:?}"),
        }
    }

    #[test]
    fn split_key_rejects_bad_segments() {
        assert!(split_key("budget.max_turns").is_ok());
        assert!(split_key("a-b_c9").is_ok());
        assert!(split_key("").is_err());
        assert!(split_key("a..b").is_err());
        assert!(split_key(".a").is_err());
        assert!(split_key("a b").is_err());
    }

    #[test]
    fn set_creates_nested_tables() {
        let mut doc = DocumentMut::new();
        set_dotted(&mut doc, "budget.max_turns", Value::from(100)).unwrap();
        let item = get_dotted(&doc, "budget.max_turns").unwrap().unwrap();
        assert_eq!(display_value(item), "100");
    }

    #[test]
    fn set_preserves_comments_and_layout() {
        let mut doc = "# keep me\n[budget]\nmax_turns = 50\n"
            .parse::<DocumentMut>()
            .unwrap();
        set_dotted(&mut doc, "budget.max_turns", Value::from(100)).unwrap();
        set_dotted(&mut doc, "goal.max_iterations", Value::from(12)).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# keep me"), "comment lost: {out}");
        assert!(out.contains("max_turns = 100"), "value not set: {out}");
        assert!(out.contains("[goal]"), "new table missing: {out}");
    }

    #[test]
    fn set_fails_when_intermediate_is_a_value() {
        let mut doc = "budget = 5\n".parse::<DocumentMut>().unwrap();
        let err = set_dotted(&mut doc, "budget.max_turns", Value::from(1)).unwrap_err();
        assert!(err.contains("already a value"), "unexpected: {err}");
    }

    #[test]
    fn get_missing_key_errors() {
        let doc = DocumentMut::new();
        let err = get_dotted(&doc, "nope.nothing").unwrap_err();
        assert!(err.contains("not set"), "unexpected: {err}");
    }

    #[test]
    fn key_eq_form_splits() {
        assert_eq!(split_key_eq("a.b=100"), Some(("a.b", "100")));
        assert_eq!(split_key_eq("novalue"), None);
        assert_eq!(split_key_eq("=x"), None);
    }

    #[test]
    fn set_rejects_unknown_top_level_key() {
        let path = scratch_config("unknown-top");
        let err = set_config_value(&path, "frobnicator.max_turns", "5").unwrap_err();
        assert!(err.contains("unknown key"), "unexpected: {err}");
        assert!(
            err.contains("\"frobnicator\""),
            "bad segment not named: {err}"
        );
        // Nothing was written: a rejected set leaves no file behind.
        assert!(!path.exists(), "rejected set wrote a file");
    }

    #[test]
    fn set_rejects_typo_in_leaf_segment() {
        let path = scratch_config("unknown-leaf");
        let err = set_config_value(&path, "goal.max_iterationz", "5").unwrap_err();
        assert!(err.contains("unknown key"), "unexpected: {err}");
        assert!(err.contains("\"max_iterationz\""), "typo not named: {err}");
        assert!(!path.exists(), "rejected set wrote a file");
    }

    #[test]
    fn set_accepts_real_keys_and_round_trips() {
        let path = scratch_config("real-keys");
        set_config_value(&path, "goal.max_iterations", "25").unwrap();
        assert_eq!(
            get_config_value(&path, "goal.max_iterations").unwrap(),
            "25"
        );
        // A second, unrelated real key in another section.
        set_config_value(&path, "budget.max_turns", "100").unwrap();
        assert_eq!(get_config_value(&path, "budget.max_turns").unwrap(), "100");
        // Profile-scoped keys (map entries) validate too.
        set_config_value(&path, "agents.coder.model", "openai/gpt-4o").unwrap();
        assert_eq!(
            get_config_value(&path, "agents.coder.model").unwrap(),
            "openai/gpt-4o"
        );
    }

    #[test]
    fn set_rejects_type_mismatch() {
        let path = scratch_config("type-mismatch");
        // goal.max_iterations is a u32; a bare string must fail loudly.
        let err = set_config_value(&path, "goal.max_iterations", "many").unwrap_err();
        assert!(!path.exists(), "type-mismatched set wrote a file");
        assert!(err.contains("goal.max_iterations"), "key not named: {err}");
    }

    #[test]
    fn set_rejects_structured_value_with_edit_direction() {
        let path = scratch_config("structured");
        let err = set_config_value(&path, "goal.max_iterations", "[1, 2]").unwrap_err();
        assert!(err.contains("config edit"), "unexpected: {err}");
        assert!(!path.exists(), "rejected set wrote a file");
    }

    #[test]
    fn atomic_write_leaves_no_torn_file_or_temp() {
        let path = scratch_config("atomic");
        let parent = path.parent().unwrap();
        set_config_value(&path, "goal.max_iterations", "42").unwrap();
        // The written file parses as clean TOML with the value in place.
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = text.parse().expect("torn config file");
        assert_eq!(parsed["goal"]["max_iterations"].as_integer(), Some(42));
        // No temp leftovers in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn atomic_write_cleans_up_on_error() {
        // Pointing the "file" at a directory makes every write fail.
        let dir = scratch_dir("atomic-err");
        let err = write_doc_atomic_at(&dir, &DocumentMut::new()).unwrap_err();
        assert!(!err.is_empty());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn validate_config_text_accepts_good_and_rejects_bad() {
        // Note: Config::validate requires a [model] section.
        let good =
            "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n[goal]\nmax_iterations = 25\n";
        assert!(validate_config_text(good).is_ok());
        assert!(validate_config_text("").is_err()); // no [model]
                                                    // TOML type error.
        let err = validate_config_text("[goal]\nmax_iterations = \"many\"\n").unwrap_err();
        assert!(err.contains("not valid config"), "unexpected: {err}");
        // Syntactically invalid TOML.
        let err = validate_config_text("[[[").unwrap_err();
        assert!(err.contains("not valid config"), "unexpected: {err}");
    }

    /// Fake editor: a shell script that overwrites its argument with `body`.
    fn fake_editor(tag: &str, body: &str) -> PathBuf {
        let dir = scratch_dir(tag);
        let script = dir.join("fake-editor.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat > \"$1\" <<'EOF'\n{body}\nEOF\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        script
    }

    #[test]
    fn edit_rejects_invalid_text_and_keeps_original() {
        let path = scratch_config("edit-reject");
        std::fs::write(&path, "[goal]\nmax_iterations = 10\n").unwrap();
        let editor = fake_editor("edit-reject", "[goal]\nmax_iterations = \"many\"\n");
        let err = run_edit_flow(&path, editor.to_str().unwrap()).unwrap_err();
        assert!(err.contains("not valid config"), "unexpected: {err}");
        assert!(err.contains("left unchanged"), "unexpected: {err}");
        // Original untouched, no backup created, no stage left behind.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[goal]\nmax_iterations = 10\n"
        );
        assert!(!path.with_extension("toml.bak").exists());
        let parent = path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "stage left behind: {leftovers:?}");
    }

    #[test]
    fn edit_accepts_valid_text_and_keeps_backup() {
        let path = scratch_config("edit-accept");
        let before =
            "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n[goal]\nmax_iterations = 10\n";
        std::fs::write(&path, before).unwrap();
        let after =
            "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n[goal]\nmax_iterations = 30\n";
        let editor = fake_editor("edit-accept", after);
        let msg = run_edit_flow(&path, editor.to_str().unwrap()).unwrap();
        assert!(msg.contains("saved"), "unexpected: {msg}");
        // (The fake editor's heredoc appends one trailing newline.)
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{after}\n")
        );
        // Pre-edit contents kept next to the file.
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            before
        );
    }

    #[test]
    fn edit_validation_failure_reports_schema_problems() {
        // A config that parses and deserializes but fails Config::validate.
        let err = validate_config_text("[model]\nprovider = \"\"\nmodel = \"x\"\n").unwrap_err();
        assert!(err.contains("failed validation"), "unexpected: {err}");
    }
}
