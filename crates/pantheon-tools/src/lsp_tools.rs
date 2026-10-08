//! LSP diagnostics toolset: feed real type-check / lint results back to the
//! model after edits. The engine (a minimal JSON-RPC LSP client +
//! diagnostics cache) lives in `pantheon-exec::lsp`; this module registers
//! the callable surface.
//!
//! The language server is long-lived, so the client is kept in a shared
//! `Arc<Mutex<Option<Arc<LspClient>>>>` and started lazily on first use,
//! keyed by server command. Tools:
//!   - lsp.open      open a file, wait for its first diagnostics batch
//!   - lsp.diagnostics  read the latest diagnostics for a file (or all)
//!   - lsp.shutdown  shut the server down and clear the cache
use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_exec::lsp::{language_for, path_uri, resolve_workspace_root, server_for, LspClient};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn err(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Execution, false, cause, remediation, "")
}

/// Options for `register_lsp_with`.
pub struct LspOptions {
    /// Root URI for the LSP `initialize` handshake (default: `file://<cwd>`).
    pub root_uri: Option<String>,
    /// Handshake + diagnostics wait timeout.
    pub timeout: Duration,
}

/// Register the LSP diagnostics toolset on a registry. The shared client
/// is created here and captured by every tool closure so the server
/// survives across calls.
pub fn register_lsp(reg: &mut ToolRegistry) {
    register_lsp_with(reg, LspOptions::default());
}

impl Default for LspOptions {
    fn default() -> Self {
        Self {
            root_uri: None,
            timeout: Duration::from_secs(20),
        }
    }
}

pub fn register_lsp_with(reg: &mut ToolRegistry, opts: LspOptions) {
    let default_root_uri: Arc<String> = Arc::new(opts.root_uri.unwrap_or_else(|| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        path_uri(&cwd)
    }));
    let timeout = opts.timeout;
    let server: Arc<Mutex<Option<Arc<LspClient>>>> = Arc::new(Mutex::new(None));
    // Tracks the root the live server was initialized against. A language
    // server only fully analyzes files that belong to its own workspace,
    // so when `lsp.open` targets a file in a *different* project we must
    // restart the server re-rooted at that project, not reuse the old one.
    let active_root: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let last_lang: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    /// Get a live client rooted at `target_root`, restarting the server
    /// when `target_root` differs from the one the current client is
    /// rooted at. `default_root_uri` is the fallback when no explicit root
    /// is supplied.
    fn ensure(
        server: &Arc<Mutex<Option<Arc<LspClient>>>>,
        active_root: &Arc<Mutex<Option<String>>>,
        lang: &str,
        target_root: Option<&str>,
        default_root_uri: &str,
        timeout: Duration,
    ) -> Result<Arc<LspClient>, PantheonError> {
        let want_root = target_root.unwrap_or(default_root_uri).to_string();
        let mut g = server.lock().unwrap_or_else(|p| p.into_inner());
        let mut rg = active_root.lock().unwrap_or_else(|p| p.into_inner());
        // Restart if there is no client yet, or the client was rooted at a
        // different workspace than the one this call targets.
        if g.is_none() || Some(want_root.as_str()) != rg.as_ref().map(|s| s.as_str()) {
            if g.is_some() {
                if let Some(old) = g.take() {
                    old.shutdown();
                }
                *rg = None;
            }
            let (program, args) = server_for(lang).ok_or_else(|| {
                PantheonError::new(
                    "LSP_NO_SERVER",
                    Layer::Execution,
                    false,
                    format!("no language server known for '{lang}'"),
                    "install the language server binary or pick a supported language",
                    "",
                )
            })?;
            *g = Some(LspClient::start(&program, &args, lang, &want_root, timeout)?);
            *rg = Some(want_root);
        }
        g.clone().ok_or_else(|| {
            PantheonError::new(
                "LSP_NO_CLIENT",
                Layer::Execution,
                false,
                "no live LSP client".to_string(),
                "call lsp.open first",
                "",
            )
        })
    }

    let mk = |name: &str,
              desc: &str,
              props: serde_json::Value,
              required: Vec<&str>|
     -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: desc.into(),
            parameters: serde_json::json!({"type":"object","properties":props,"required":required}),
        }
    };

    let s1 = server.clone();
    let l1 = last_lang.clone();
    let a1 = active_root.clone();
    let d1 = default_root_uri.clone();
    reg.register(
        mk(
            "lsp.open",
            "Open a source file in the language server and wait for its first diagnostics batch. Starts the server on demand and roots it at the file's project. Returns the diagnostics (may be empty if clean).",
            serde_json::json!({
                "path": {"type":"string","description":"Absolute or workspace-relative file path"},
                "language": {"type":"string","description":"Optional language override (default: inferred from extension)"},
                "wait_secs": {"type":"integer","description":"Diagnostics wait budget (default 15)"},
                "root": {"type":"string","description":"Optional explicit project root directory (default: auto-detected by walking up from the file)"},
            }),
            vec!["path"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let path = v
                .get("path")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("TOOL_BAD_ARGS", "missing 'path'".to_string(), "check tool arguments"))?;
            let lang = v
                .get("language")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    Path::new(path)
                        .extension()
                        .and_then(|e| language_for(e.to_str()?).map(|s| s.to_string()))
                })
                .ok_or_else(|| err("LSP_NO_LANG", format!("cannot infer language for '{path}'"), "pass 'language' explicitly"))?;
            *l1.lock().unwrap_or_else(|p| p.into_inner()) = Some(lang.clone());
            let wait_secs = v.get("wait_secs").and_then(|x| x.as_u64()).unwrap_or(15);
            let cpath = PathBuf::from(path);
            // Resolve the project root: an explicit `root` wins, otherwise
            // walk up from the file to its nearest manifest. This is what
            // lets the tool analyze files in projects other than the one
            // pantheon was launched from.
            let target_root = match v.get("root").and_then(|x| x.as_str()) {
                Some(explicit) => Some(path_uri(Path::new(explicit))),
                None => Some(path_uri(&resolve_workspace_root(&cpath))),
            };
            let client = ensure(&s1, &a1, &lang, target_root.as_deref(), &d1, timeout)?;
            let text = std::fs::read_to_string(&cpath).map_err(|e| {
                err(
                    "LSP_READ",
                    format!("read {path}: {e}"),
                    "check the file path",
                )
            })?;
            client.open_document(&cpath, &lang, &text)?;
            let uri = path_uri(&cpath);
            let diags = client.wait_diagnostics(&uri, Duration::from_secs(wait_secs))?;
            match diags {
                Some(d) if d.count > 0 => Ok(serde_json::to_string_pretty(&d).unwrap_or(d.uri)),
                Some(d) => {
                    // An empty batch by the deadline means the server
                    // emitted "no diagnostics yet" - it may still be
                    // analyzing (rust-analyzer's first batch is empty).
                    // Report that honestly instead of claiming the file is
                    // clean.
                    Ok(format!(
                        "no diagnostics for {uri} within {wait_secs}s; the server may still be analyzing (retry, or increase wait_secs). last batch: {} error(s)",
                        d.count
                    ))
                }
                None => Ok(format!(
                    "no diagnostics for {uri} within {wait_secs}s (server may not be ready or file is clean)"
                )),
            }
        },
    );

    let s2 = server.clone();
    let l2 = last_lang.clone();
    let a2 = active_root.clone();
    let d2 = default_root_uri.clone();
    reg.register(
        mk(
            "lsp.diagnostics",
            "Read the latest LSP diagnostics for one file ('path') or all cached files ('path' omitted). Read-only against the diagnostics cache; no file access.",
            serde_json::json!({
                "path": {"type":"string","description":"File to read diagnostics for; omit for all"},
                "language": {"type":"string","description":"Language whose server produced the cache"},
            }),
            vec![],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let lang = v
                .get("language")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .or_else(|| l2.lock().unwrap_or_else(|p| p.into_inner()).clone())
                .ok_or_else(|| err("LSP_NO_LANG", "no language in scope; pass 'language'".to_string(), "pass 'language' explicitly"))?;
            let client = ensure(&s2, &a2, &lang, None, &d2, timeout)?;
            let maybe_uri = v.get("path").and_then(|x| x.as_str()).map(|p| path_uri(Path::new(p)));
            let out: Vec<_> = match maybe_uri {
                Some(uri) => client.diagnostics(&uri).into_iter().collect(),
                None => client.all_diagnostics(),
            };
            serde_json::to_string_pretty(&out)
                .map_err(|e| err("LSP_JSON", e.to_string(), "internal: serialize diagnostics"))
        },
    );

    let s3 = server.clone();
    let a3 = active_root.clone();
    reg.register(
        mk(
            "lsp.shutdown",
            "Shut the LSP server down cleanly and clear the diagnostics cache.",
            serde_json::json!({}),
            vec![],
        ),
        Capability::ShellExecute,
        move |_args| {
            let mut g = s3.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(client) = g.take() {
                client.shutdown();
                *a3.lock().unwrap_or_else(|p| p.into_inner()) = None;
                Ok("LSP server shut down; diagnostics cache cleared".to_string())
            } else {
                Ok("no live LSP server; nothing to shut down".to_string())
            }
        },
    );
}
