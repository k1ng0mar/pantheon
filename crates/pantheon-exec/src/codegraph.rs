//! A small, native Rust codebase graph.
//!
//! `syn` parses each `.rs` file under a root; the visitor records symbols
//! (functions, methods, structs, enums, traits, modules) and call edges
//! (`foo()` / `x.foo()`). The result is a cacheable JSON graph with
//! search, caller, callee, and overview queries - enough for an agent to
//! orient in a repository without a language server or an external MCP
//! index.
//!
//! Deliberately static and lexical: calls are resolved by the final path
//! segment, so overloads and trait dispatch can produce several matches.
//! That is useful as a "who mentions/calls this name" index, not as a
//! compiler-grade reference graph. `rust-analyzer` remains the authority
//! for type-aware navigation; this fills the gap when it is unavailable.

use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the repository path and try again",
        "",
    )
}

/// One symbol in the graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeNode {
    pub id: String,
    pub name: String,
    pub qualified: String,
    /// `function`, `method`, `struct`, `enum`, `trait`, `module`, `type`.
    pub kind: String,
    pub file: String,
    pub line: usize,
}

/// One call edge. `to_name` is the lexical callee name; resolution to a
/// concrete node is done at query time so ambiguous names are not lost.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeEdge {
    pub from: String,
    pub from_name: String,
    pub to_name: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeGraph {
    pub root: String,
    pub generated_ms: u64,
    /// Fingerprint of indexed Rust file paths, sizes, and modification times.
    /// Empty on older cached indexes, which forces one rebuild after upgrade.
    #[serde(default)]
    pub source_fingerprint: String,
    pub files: usize,
    pub parse_errors: usize,
    pub nodes: Vec<CodeNode>,
    pub edges: Vec<CodeEdge>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallHit {
    pub node: CodeNode,
    pub edge: CodeEdge,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub root: String,
    pub files: usize,
    pub parse_errors: usize,
    pub symbols: usize,
    pub calls: usize,
    pub by_kind: std::collections::BTreeMap<String, usize>,
    pub top_directories: Vec<(String, usize)>,
}

impl CodeGraph {
    /// Parse every Rust file under `root` (canonicalized). Unreadable or
    /// unparseable files are counted, not fatal: a graph over a partially
    /// generated tree is still useful.
    pub fn build(root: impl AsRef<Path>) -> Result<Self, PantheonError> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|e| {
            err(
                "CODEGRAPH_ROOT",
                format!("canonicalize {}: {e}", root.as_ref().display()),
            )
        })?;
        let mut files = Vec::new();
        walk(&root, &mut files, 0)?;
        files.sort();
        let source_fingerprint = fingerprint_files(&root, &files);
        let mut graph = Self {
            root: root.to_string_lossy().to_string(),
            generated_ms: 0,
            source_fingerprint,
            files: files.len(),
            parse_errors: 0,
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        for file in files {
            let rel = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            let src = match std::fs::read_to_string(&file) {
                Ok(s) => s,
                Err(_) => {
                    graph.parse_errors += 1;
                    continue;
                }
            };
            let ast = match syn::parse_file(&src) {
                Ok(a) => a,
                Err(_) => {
                    graph.parse_errors += 1;
                    continue;
                }
            };
            let mut c = Collector::new(rel);
            c.visit_file(&ast);
            graph.nodes.extend(c.nodes);
            graph.edges.extend(c.edges);
        }
        // Stable, duplicate-free output. Two identical call edges in one
        // function (e.g. two `log()` calls) collapse to one lexical edge;
        // line numbers keep distinct call sites distinct.
        graph.nodes.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.line.cmp(&b.line))
                .then(a.id.cmp(&b.id))
        });
        graph.edges.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.line.cmp(&b.line))
                .then(a.from.cmp(&b.from))
                .then(a.to_name.cmp(&b.to_name))
        });
        graph.edges.dedup();
        graph.generated_ms = now_ms();
        Ok(graph)
    }

    /// Check whether this index still describes the current Rust source
    /// tree. The metadata fingerprint catches normal edits; the five-minute
    /// age bound also catches edits that preserve size and modification time.
    pub fn is_current_for(&self, root: impl AsRef<Path>) -> bool {
        let root = match std::fs::canonicalize(root.as_ref()) {
            Ok(root) => root,
            Err(_) => return false,
        };
        if Path::new(&self.root) != root || self.source_fingerprint.is_empty() {
            return false;
        }
        let age_ms = now_ms().saturating_sub(self.generated_ms);
        if age_ms > 300_000 {
            return false;
        }
        let mut files = Vec::new();
        if walk(&root, &mut files, 0).is_err() {
            return false;
        }
        files.sort();
        fingerprint_files(&root, &files) == self.source_fingerprint
    }

    /// Legacy single-index location. Prefer [`Self::cache_path_for`] when
    /// the repository root is known: one data dir may serve several
    /// checkouts (main tree + worktrees), and a single file would thrash.
    pub fn cache_path(state_dir: &Path) -> PathBuf {
        state_dir.join("codegraph").join("index.json")
    }

    /// Per-repository cache path: `<state>/codegraph/<root-hash>.json`.
    /// The root is canonicalized first so `foo` and `/abs/foo` share one
    /// entry.
    pub fn cache_path_for(state_dir: &Path, root: &Path) -> PathBuf {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let hash = crate::safewrite::fnv1a_hex(canonical.to_string_lossy().as_bytes());
        state_dir
            .join("codegraph")
            .join(format!("{}.json", &hash[..16]))
    }

    pub fn save(&self, path: &Path) -> Result<(), PantheonError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                err(
                    "CODEGRAPH_MKDIR",
                    format!("create {}: {e}", parent.display()),
                )
            })?;
        }
        let raw = serde_json::to_string(self)
            .map_err(|e| err("CODEGRAPH_JSON", format!("serialize index: {e}")))?;
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, raw)
            .and_then(|_| std::fs::rename(&tmp, path))
            .map_err(|e| err("CODEGRAPH_WRITE", format!("write {}: {e}", path.display())))
    }

    pub fn load(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<&CodeNode> {
        let q = query.to_ascii_lowercase();
        self.nodes
            .iter()
            .filter(|n| {
                n.name.to_ascii_lowercase().contains(&q)
                    || n.qualified.to_ascii_lowercase().contains(&q)
                    || n.file.to_ascii_lowercase().contains(&q)
            })
            .take(limit)
            .collect()
    }

    /// Call sites whose lexical callee matches `symbol` (exact final
    /// segment, case-sensitive). Returns the caller node plus the edge.
    pub fn callers(&self, symbol: &str, limit: usize) -> Vec<CallHit> {
        let by_id: std::collections::HashMap<&str, &CodeNode> =
            self.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        self.edges
            .iter()
            .filter(|e| e.to_name == symbol)
            .filter_map(|e| {
                by_id.get(e.from.as_str()).map(|n| CallHit {
                    node: (*n).clone(),
                    edge: e.clone(),
                })
            })
            .take(limit)
            .collect()
    }

    /// Outgoing lexical call edges from nodes named `symbol`. Each edge
    /// carries `to_name` (the callee's final path segment) because a
    /// lexical graph cannot resolve overloads or trait dispatch.
    pub fn callees(&self, symbol: &str, limit: usize) -> Vec<CodeEdge> {
        let from: std::collections::HashSet<&str> = self
            .nodes
            .iter()
            .filter(|n| n.name == symbol || n.qualified.ends_with(&format!("::{symbol}")))
            .map(|n| n.id.as_str())
            .collect();
        self.edges
            .iter()
            .filter(|e| from.contains(e.from.as_str()))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn overview(&self) -> Overview {
        let mut by_kind = std::collections::BTreeMap::new();
        for n in &self.nodes {
            *by_kind.entry(n.kind.clone()).or_insert(0) += 1;
        }
        let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for n in &self.nodes {
            let dir = n
                .file
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_else(|| ".".to_string());
            *dirs.entry(dir).or_insert(0) += 1;
        }
        let mut top_directories: Vec<(String, usize)> = dirs.into_iter().collect();
        top_directories.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        top_directories.truncate(20);
        Overview {
            root: self.root.clone(),
            files: self.files,
            parse_errors: self.parse_errors,
            symbols: self.nodes.len(),
            calls: self.edges.len(),
            by_kind,
            top_directories,
        }
    }
}

fn fingerprint_files(root: &Path, files: &[PathBuf]) -> String {
    let mut bytes = Vec::new();
    for file in files {
        let rel = file.strip_prefix(root).unwrap_or(file).to_string_lossy();
        bytes.extend_from_slice(rel.as_bytes());
        bytes.push(0);
        match std::fs::metadata(file) {
            Ok(meta) => {
                bytes.extend_from_slice(&meta.len().to_le_bytes());
                let modified_ns = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .unwrap_or_default();
                bytes.extend_from_slice(&modified_ns.to_le_bytes());
            }
            Err(_) => {
                // An unreadable or disappearing file must not look like a
                // valid cached source entry.
                bytes.extend_from_slice(&u64::MAX.to_le_bytes());
                bytes.extend_from_slice(&u128::MAX.to_le_bytes());
            }
        }
        bytes.push(0xff);
    }
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<(), PantheonError> {
    if depth > 32 || out.len() >= 20_000 {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| err("CODEGRAPH_READ", format!("read {}: {e}", dir.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                ".git"
                    | "target"
                    | "node_modules"
                    | "vendor"
                    | "dist"
                    | "build"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
            ) {
                continue;
            }
            walk(&path, out, depth + 1)?;
        } else if file_type.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some("rs")
            && std::fs::metadata(&path)
                .map(|m| m.len() <= 2 * 1024 * 1024)
                .unwrap_or(false)
        {
            out.push(path);
        }
    }
    Ok(())
}

struct Collector {
    file: String,
    module: Vec<String>,
    impl_type: Option<String>,
    current_fn: Option<(String, String)>,
    nodes: Vec<CodeNode>,
    edges: Vec<CodeEdge>,
    seq: usize,
}

impl Collector {
    fn new(file: String) -> Self {
        let module = module_from_file(&file);
        Self {
            file,
            module,
            impl_type: None,
            current_fn: None,
            nodes: Vec::new(),
            edges: Vec::new(),
            seq: 0,
        }
    }

    fn qualified(&self, name: &str) -> String {
        let mut parts = self.module.clone();
        if let Some(t) = &self.impl_type {
            parts.push(t.clone());
        }
        parts.push(name.to_string());
        parts.join("::")
    }

    fn add_node(&mut self, name: &str, kind: &str, line: usize) {
        self.seq += 1;
        let qualified = self.qualified(name);
        self.nodes.push(CodeNode {
            id: format!("{}:{}:{}:{}", self.file, line, self.seq, qualified),
            name: name.to_string(),
            qualified,
            kind: kind.to_string(),
            file: self.file.clone(),
            line,
        });
    }

    fn add_edge(&mut self, to: &str, line: usize) {
        let Some((from, from_name)) = self.current_fn.clone() else {
            return;
        };
        self.edges.push(CodeEdge {
            from,
            from_name,
            to_name: to.to_string(),
            file: self.file.clone(),
            line,
        });
    }
}

fn module_from_file(file: &str) -> Vec<String> {
    let mut parts: Vec<String> = file
        .trim_end_matches(".rs")
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(pos) = parts.iter().position(|p| p == "src") {
        parts.drain(..=pos);
    }
    if matches!(
        parts.last().map(String::as_str),
        Some("lib") | Some("main") | Some("mod")
    ) {
        parts.pop();
    }
    parts
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        let name = i.ident.to_string();
        self.add_node(&name, "module", i.span().start().line);
        self.module.push(name);
        visit::visit_item_mod(self, i);
        self.module.pop();
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        let name = i.sig.ident.to_string();
        let line = i.span().start().line;
        self.add_node(&name, "function", line);
        let id = self.nodes.last().map(|n| n.id.clone()).unwrap_or_default();
        let prev = self.current_fn.replace((id, name));
        visit::visit_item_fn(self, i);
        self.current_fn = prev;
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.add_node(&i.ident.to_string(), "struct", i.span().start().line);
        visit::visit_item_struct(self, i);
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.add_node(&i.ident.to_string(), "enum", i.span().start().line);
        visit::visit_item_enum(self, i);
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        let name = i.ident.to_string();
        self.add_node(&name, "trait", i.span().start().line);
        let prev = self.impl_type.replace(name);
        visit::visit_item_trait(self, i);
        self.impl_type = prev;
    }

    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.add_node(&i.ident.to_string(), "type", i.span().start().line);
        visit::visit_item_type(self, i);
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let prev = self.impl_type.clone();
        if let Some(ty) = type_name(&i.self_ty) {
            self.impl_type = Some(ty);
        }
        visit::visit_item_impl(self, i);
        self.impl_type = prev;
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        let name = i.sig.ident.to_string();
        self.add_node(&name, "method", i.span().start().line);
        let id = self.nodes.last().map(|n| n.id.clone()).unwrap_or_default();
        let prev = self.current_fn.replace((id, name));
        visit::visit_impl_item_fn(self, i);
        self.current_fn = prev;
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        let name = i.sig.ident.to_string();
        self.add_node(&name, "method", i.span().start().line);
        let id = self.nodes.last().map(|n| n.id.clone()).unwrap_or_default();
        let prev = self.current_fn.replace((id, name));
        visit::visit_trait_item_fn(self, i);
        self.current_fn = prev;
    }

    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        if let Some(name) = callee_name(&i.func) {
            self.add_edge(&name, i.span().start().line);
        }
        visit::visit_expr_call(self, i);
    }

    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        self.add_edge(&i.method.to_string(), i.span().start().line);
        visit::visit_expr_method_call(self, i);
    }
}

fn callee_name(e: &syn::Expr) -> Option<String> {
    match e {
        syn::Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        syn::Expr::Paren(p) => callee_name(&p.expr),
        syn::Expr::Group(g) => callee_name(&g.expr),
        _ => None,
    }
}

fn type_name(ty: &syn::Type) -> Option<String> {
    let syn::Type::Path(p) = ty else {
        return None;
    };
    p.path.segments.last().map(|s| s.ident.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn indexes_symbols_and_calls() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "src/lib.rs",
            r#"
pub fn run() {
    helper();
    thing.go();
}

fn helper() {}

struct Thing;
impl Thing {
    fn go(&self) {}
}
"#,
        );
        let g = CodeGraph::build(tmp.path()).unwrap();
        assert_eq!(g.files, 1);
        assert_eq!(g.parse_errors, 0);
        assert!(g
            .nodes
            .iter()
            .any(|n| n.name == "run" && n.kind == "function"));
        assert!(g.nodes.iter().any(|n| n.name == "go" && n.kind == "method"));
        assert!(g
            .nodes
            .iter()
            .any(|n| n.name == "Thing" && n.kind == "struct"));
        let hits = g.callers("helper", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node.name, "run");
        let hits = g.callers("go", 10);
        assert_eq!(hits.len(), 1);
        let hits = g.callees("run", 10);
        assert_eq!(hits.len(), 2);
        assert!(g.overview().symbols >= 4);
    }

    #[test]
    fn skips_target_and_survives_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn ok() {}\n");
        write(tmp.path(), "src/broken.rs", "this is not rust {{{\n");
        write(tmp.path(), "target/generated.rs", "pub fn nope() {}\n");
        let g = CodeGraph::build(tmp.path()).unwrap();
        assert_eq!(g.files, 2);
        assert_eq!(g.parse_errors, 1);
        assert!(g.nodes.iter().any(|n| n.name == "ok"));
        assert!(!g.nodes.iter().any(|n| n.name == "nope"));
        assert!(g.search("ok", 10).iter().any(|n| n.name == "ok"));
    }

    #[test]
    fn cache_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn cached() {}\n");
        let g = CodeGraph::build(tmp.path()).unwrap();
        let path = CodeGraph::cache_path(tmp.path());
        g.save(&path).unwrap();
        let loaded = CodeGraph::load(&path).unwrap();
        assert_eq!(loaded.nodes.len(), g.nodes.len());
        assert!(loaded.search("cached", 10).len() == 1);
    }

    #[test]
    fn per_repository_cache_paths_do_not_collide() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let pa = CodeGraph::cache_path_for(state.path(), a.path());
        let pb = CodeGraph::cache_path_for(state.path(), b.path());
        assert_ne!(pa, pb);
        assert_eq!(pa, CodeGraph::cache_path_for(state.path(), a.path()));
        assert_eq!(
            pa.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("codegraph"))
        );
    }
}
