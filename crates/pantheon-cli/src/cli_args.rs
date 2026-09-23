//! Shared CLI argument helpers. One parser for every verb: `--flag value`
//! and `--flag=value` both work, positional args are those that do not
//! start with `--`.
use std::collections::HashMap;

/// Parsed arguments: positional values in order plus a flag map (last
/// occurrence wins).
pub struct Args {
    pub positionals: Vec<String>,
    flags: HashMap<String, String>,
}

impl Args {
    pub fn parse(raw: &[String]) -> Self {
        let mut positionals = Vec::new();
        let mut flags = HashMap::new();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            if let Some(stripped) = a.strip_prefix("--") {
                if let Some((k, v)) = stripped.split_once('=') {
                    flags.insert(k.to_string(), v.to_string());
                } else if let Some(v) = raw.get(i + 1) {
                    flags.insert(stripped.to_string(), v.clone());
                    i += 1;
                } else {
                    // Boolean flag at end of argv.
                    flags.insert(stripped.to_string(), String::new());
                }
            } else {
                positionals.push(a.clone());
            }
            i += 1;
        }
        Self { positionals, flags }
    }
    pub fn flag(&self, name: &str) -> Option<String> {
        self.flags.get(name).filter(|v| !v.is_empty()).cloned()
    }
    /// True when the flag appeared, even without a value (boolean flags).
    #[allow(dead_code)] // part of the shared parser surface; used by upcoming verb migrations
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
    pub fn positional(&self, index: usize) -> Option<String> {
        self.positionals.get(index).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn both_flag_forms_parse() {
        let a = Args::parse(&raw(&[
            "--model",
            "m1",
            "--provider=openai",
            "pos1",
            "--dry-run",
        ]));
        assert_eq!(a.flag("model").as_deref(), Some("m1"));
        assert_eq!(a.flag("provider").as_deref(), Some("openai"));
        assert_eq!(a.positional(0).as_deref(), Some("pos1"));
        assert!(a.has("dry-run"));
        assert_eq!(a.flag("dry-run"), None, "no value = not a value flag");
    }

    #[test]
    fn last_occurrence_wins() {
        let a = Args::parse(&raw(&["--model", "a", "--model", "b"]));
        assert_eq!(a.flag("model").as_deref(), Some("b"));
    }
}
