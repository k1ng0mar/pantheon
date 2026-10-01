//! Shared raw-CDP driver over `chromiumoxide` (=0.9.1, pinned).
//!
//! [`CdpDriver`] translates the canonical browser argv (see
//! [`super::backend`]) into CDP commands. It is shared by three
//! backends: the native local backend ([`super::native`], launched
//! Chrome), Steel ([`super::steel`]) and Browserbase
//! ([`super::browserbase`]) — both of which hand us a websocket URL —
//! and Lightpanda ([`super::lightpanda`], extraction-only subset).
//!
//! ## CDP schema drift
//!
//! Chrome's CDP schema moves under the client (Chrome 142 broke
//! chromiumoxide 0.6's protocol types). Two defenses:
//!
//! * the crate version is pinned exact (`=0.9.1` in `Cargo.toml`);
//! * unknown protocol *events* never hard-fail: chromiumoxide 0.9
//!   deserializes unrecognized event methods into
//!   `CdpEvent::Other(serde_json::Value)` and the handler keeps going —
//!   verified in the vendored source
//!   (`chromiumoxide_cdp-0.9.1/src/cdp.rs`, the `_ => CdpEvent::Other(..)`
//!   arm of the `CdpEventMessage` deserializer). Command responses ignore
//!   unknown fields by default serde behavior.
//!
//! ## Ref model
//!
//! `snapshot` walks `Accessibility.getFullAXTree` and assigns versioned
//! refs (`@v{n}:e{m}`) anchored on `backendDOMNodeId`. Refs invalidate on
//! every new snapshot; using a ref from an older version is
//! [`BrowserError::StaleRef`], mirroring gsd-browser semantics.
//!
//! ## Threading
//!
//! chromiumoxide is async (tokio). The [`BrowserBackend`] trait is sync,
//! so the driver owns one shared multi-thread tokio runtime and
//! `block_on`s each call. The handler future is parked on a spawned task
//! per connection (chromiumoxide requires the handler to be polled).

use super::error::BrowserError;
use chromiumoxide::cdp::browser_protocol::{accessibility, dom, page as cdp_page};
use chromiumoxide::cdp::js_protocol::runtime;
use chromiumoxide::error::CdpError;
use chromiumoxide::{Browser, BrowserConfig, Handler, Page};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

/// Per-command operation timeout default (seconds).
pub const DEFAULT_OP_TIMEOUT_SECS: u64 = 120;

/// Poll interval for `wait-for` conditions.
const WAIT_POLL: Duration = Duration::from_millis(250);
/// Upper bound for one `wait-for` invocation.
const WAIT_MAX: Duration = Duration::from_secs(30);

/// One shared tokio runtime for all CDP drivers in the process.
pub(crate) fn shared_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("pantheon-cdp")
            .build()
            .expect("failed to build CDP tokio runtime")
    })
}

/// One versioned ref target: an AX node anchored on its backend DOM id.
#[derive(Debug, Clone)]
pub struct RefTarget {
    pub backend_node_id: i64,
    pub role: String,
    pub name: String,
}

/// Versioned ref table for one session page.
#[derive(Debug, Default)]
pub struct RefTable {
    version: u64,
    entries: HashMap<String, RefTarget>,
}

impl RefTable {
    /// Rebuild from a fresh AX snapshot; returns the new version and the
    /// JSON-serializable element list.
    pub fn rebuild(&mut self, nodes: &[(i64, String, String)]) -> (u64, Vec<serde_json::Value>) {
        self.version += 1;
        self.entries.clear();
        let v = self.version;
        let mut out = Vec::with_capacity(nodes.len());
        for (i, (backend_node_id, role, name)) in nodes.iter().enumerate() {
            let r = format!("@v{v}:e{i}");
            self.entries.insert(
                r.clone(),
                RefTarget {
                    backend_node_id: *backend_node_id,
                    role: role.clone(),
                    name: name.clone(),
                },
            );
            out.push(serde_json::json!({
                "ref": r,
                "role": role,
                "name": name,
            }));
        }
        (v, out)
    }

    /// Resolve a ref string, enforcing version freshness.
    pub fn resolve(&self, r: &str) -> Result<&RefTarget, BrowserError> {
        let v: u64 = r
            .strip_prefix("@v")
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| BrowserError::StaleRef {
                message: format!("malformed ref '{r}': take a fresh snapshot"),
            })?;
        if v != self.version {
            return Err(BrowserError::StaleRef {
                message: format!(
                    "ref {r} is from snapshot v{v}, current is v{}: take a fresh snapshot",
                    self.version
                ),
            });
        }
        self.entries.get(r).ok_or_else(|| BrowserError::StaleRef {
            message: format!("unknown ref '{r}': take a fresh snapshot"),
        })
    }
}

/// Per-session driver state: one page plus its ref table.
struct SessionState {
    page: Page,
    refs: RefTable,
}

struct DriverInner {
    browser: Option<Browser>,
    /// Parked handler-poll task; dropped with the driver.
    _handler_task: Option<tokio::task::JoinHandle<()>>,
    sessions: HashMap<String, SessionState>,
}

/// How the driver reaches a browser.
enum Connector {
    Launch {
        chrome_path: Option<PathBuf>,
        headless: bool,
        request_timeout: Duration,
    },
    Connect {
        url: String,
    },
}

/// Raw-CDP driver shared by the native, Steel, Browserbase and
/// Lightpanda backends. Connection is lazy: nothing dials out until the
/// first command.
pub struct CdpDriver {
    connector: Connector,
    /// Async mutex: `Browser::new_page` needs `&Browser` across an await,
    /// which a std mutex cannot lend. One lock guards init + page
    /// creation; command dispatch itself runs unlocked.
    inner: tokio::sync::Mutex<DriverInner>,
    op_timeout_secs: u64,
    /// Backend name used in [`BrowserError::UnsupportedCommand`].
    backend_name: String,
}

impl CdpDriver {
    /// Lazy local-Chrome driver (chromiumoxide launch).
    pub fn launch_lazy(
        chrome_path: Option<PathBuf>,
        headless: bool,
        backend_name: impl Into<String>,
    ) -> Self {
        Self {
            connector: Connector::Launch {
                chrome_path,
                headless,
                request_timeout: Duration::from_secs(DEFAULT_OP_TIMEOUT_SECS),
            },
            inner: tokio::sync::Mutex::new(DriverInner {
                browser: None,
                _handler_task: None,
                sessions: HashMap::new(),
            }),
            op_timeout_secs: DEFAULT_OP_TIMEOUT_SECS,
            backend_name: backend_name.into(),
        }
    }

    /// Lazy websocket driver (Steel/Browserbase/Lightpanda session URL).
    /// Returns `Err` immediately when the URL is empty — fail fast at
    /// construction, not on first tool call.
    pub fn connect_lazy(
        url: String,
        backend_name: impl Into<String>,
    ) -> Result<Self, BrowserError> {
        if url.trim().is_empty() {
            return Err(BrowserError::Failed {
                argv: vec![],
                exit: None,
                stderr: "CDP websocket URL is empty".into(),
            });
        }
        Ok(Self {
            connector: Connector::Connect { url },
            inner: tokio::sync::Mutex::new(DriverInner {
                browser: None,
                _handler_task: None,
                sessions: HashMap::new(),
            }),
            op_timeout_secs: DEFAULT_OP_TIMEOUT_SECS,
            backend_name: backend_name.into(),
        })
    }

    pub fn with_op_timeout(mut self, secs: u64) -> Self {
        self.op_timeout_secs = secs.max(1);
        if let Connector::Launch {
            request_timeout, ..
        } = &mut self.connector
        {
            *request_timeout = Duration::from_secs(self.op_timeout_secs);
        }
        self
    }

    fn cdp_err(&self, argv: &[String], e: impl std::fmt::Display) -> BrowserError {
        // Never include the websocket URL (it can carry an API key query
        // param, e.g. Steel's `&apiKey=`).
        BrowserError::Failed {
            argv: argv.to_vec(),
            exit: None,
            stderr: format!("CDP error: {e}"),
        }
    }

    /// Launch or connect, returning the browser and its event handler.
    async fn start_browser(&self, argv: &[String]) -> Result<(Browser, Handler), BrowserError> {
        let start = async {
            match &self.connector {
                Connector::Launch {
                    chrome_path,
                    headless,
                    request_timeout,
                } => {
                    let mut builder = BrowserConfig::builder()
                        .request_timeout(*request_timeout)
                        .no_sandbox();
                    if let Some(p) = chrome_path {
                        builder = builder.chrome_executable(p);
                    }
                    if !headless {
                        builder = builder.with_head();
                    }
                    let config = builder
                        .build()
                        .map_err(|e| CdpError::msg(format!("invalid browser config: {e}")))?;
                    Browser::launch(config).await
                }
                Connector::Connect { url } => Browser::connect(url.clone()).await,
            }
        };
        start.await.map_err(|e| self.cdp_err(argv, e))
    }

    /// Get (creating) the page for a Pantheon session, initializing the
    /// browser on first use.
    async fn session_page(&self, session: &str, argv: &[String]) -> Result<Page, BrowserError> {
        let mut inner = self.inner.lock().await;
        if let Some(s) = inner.sessions.get(session) {
            return Ok(s.page.clone());
        }
        if inner.browser.is_none() {
            let (browser, mut handler) = self.start_browser(argv).await?;
            // chromiumoxide requires the handler future to be polled;
            // park it on a task for the connection's lifetime. Unknown
            // CDP events arrive as `CdpEvent::Other` and are ignored —
            // schema drift never wedges the driver.
            inner._handler_task = Some(tokio::spawn(async move {
                while handler.next().await.is_some() {}
            }));
            inner.browser = Some(browser);
        }
        let page = inner
            .browser
            .as_ref()
            .expect("browser initialized above")
            .new_page("about:blank")
            .await
            .map_err(|e| self.cdp_err(argv, e))?;
        inner.sessions.insert(
            session.to_string(),
            SessionState {
                page: page.clone(),
                refs: RefTable::default(),
            },
        );
        Ok(page)
    }

    /// Close the page for `session`, dropping its ref table.
    pub fn close_session(&self, session: &str) {
        let page = shared_runtime().block_on(async {
            self.inner
                .lock()
                .await
                .sessions
                .remove(session)
                .map(|s| s.page)
        });
        if let Some(page) = page {
            let _ = shared_runtime().block_on(page.close());
        }
    }

    /// Execute one canonical command in `session`.
    pub fn execute(
        &self,
        argv: &[String],
        session: &str,
    ) -> Result<serde_json::Value, BrowserError> {
        let cmd = argv.first().map(String::as_str).unwrap_or("");
        if cmd.is_empty() {
            return Err(self.unsupported("(empty command)"));
        }
        let timeout = Duration::from_secs(self.op_timeout_secs);
        shared_runtime().block_on(async {
            let page = self.session_page(session, argv).await?;
            tokio::time::timeout(timeout, self.dispatch(&page, session, argv))
                .await
                .map_err(|_| BrowserError::Timeout {
                    argv: argv.to_vec(),
                    secs: self.op_timeout_secs,
                })?
        })
    }
    fn unsupported(&self, command: &str) -> BrowserError {
        BrowserError::UnsupportedCommand {
            command: command.to_string(),
            backend: self.backend_name.clone(),
        }
    }

    async fn dispatch(
        &self,
        page: &Page,
        session: &str,
        argv: &[String],
    ) -> Result<serde_json::Value, BrowserError> {
        let err = |e: chromiumoxide::error::CdpError| self.cdp_err(argv, e);
        match argv[0].as_str() {
            "navigate" => {
                let url = argv
                    .get(1)
                    .ok_or_else(|| self.unsupported("navigate (missing url)"))?;
                let params = cdp_page::NavigateParams::builder()
                    .url(url.clone())
                    .build()
                    .map_err(|e| self.cdp_err(argv, e))?;
                page.goto(params).await.map_err(err)?;
                Ok(serde_json::json!({"ok": true, "url": url}))
            }
            "back" => {
                self.history_step(page, argv, -1).await?;
                Ok(serde_json::json!({"ok": true}))
            }
            "forward" => {
                self.history_step(page, argv, 1).await?;
                Ok(serde_json::json!({"ok": true}))
            }
            "reload" => {
                page.reload().await.map_err(err)?;
                Ok(serde_json::json!({"ok": true}))
            }
            "snapshot" => self.snapshot(page, session, argv).await,
            "click-ref" => {
                let r = ref_arg(argv)?;
                let oid = self.resolve_ref_object_id(page, session, argv, r).await?;
                self.call_fn_on(page, argv, &oid, "(el) => el.click()", None)
                    .await?;
                Ok(serde_json::json!({"ok": true}))
            }
            "hover-ref" => {
                let r = ref_arg(argv)?;
                let oid = self.resolve_ref_object_id(page, session, argv, r).await?;
                self.call_fn_on(
                    page,
                    argv,
                    &oid,
                    "(el) => { el.scrollIntoView({block:'nearest'}); el.dispatchEvent(new MouseEvent('mouseover',{bubbles:true})); }",
                    None,
                )
                .await?;
                Ok(serde_json::json!({"ok": true}))
            }
            "fill-ref" => {
                let r = ref_arg(argv)?;
                let oid = self.resolve_ref_object_id(page, session, argv, r).await?;
                let text = argv.get(2).cloned().unwrap_or_default();
                self.call_fn_on(
                    page,
                    argv,
                    &oid,
                    "(el, text) => { el.focus(); el.value = text; el.dispatchEvent(new Event('input',{bubbles:true})); el.dispatchEvent(new Event('change',{bubbles:true})); }",
                    Some(text),
                )
                .await?;
                Ok(serde_json::json!({"ok": true}))
            }
            "click" => {
                let sel = argv
                    .get(1)
                    .ok_or_else(|| self.unsupported("click (missing selector)"))?;
                page.find_element(sel.clone())
                    .await
                    .map_err(err)?
                    .click()
                    .await
                    .map_err(err)?;
                Ok(serde_json::json!({"ok": true}))
            }
            "type" => {
                let sel = argv
                    .get(1)
                    .ok_or_else(|| self.unsupported("type (missing selector)"))?;
                let text = argv.get(2).cloned().unwrap_or_default();
                page.find_element(sel.clone())
                    .await
                    .map_err(err)?
                    .type_str(&text)
                    .await
                    .map_err(err)?;
                Ok(serde_json::json!({"ok": true}))
            }
            "press" => {
                let key = argv
                    .get(1)
                    .ok_or_else(|| self.unsupported("press (missing key)"))?;
                page.find_element("body".to_string())
                    .await
                    .map_err(err)?
                    .press_key(key)
                    .await
                    .map_err(err)?;
                Ok(serde_json::json!({"ok": true}))
            }
            "wait-for" => self.wait_for(page, argv).await,
            "extract" => self.extract(page, argv).await,
            "screenshot" => self.screenshot(page, argv).await,
            "page-source" => {
                let html = page.content().await.map_err(err)?;
                Ok(serde_json::json!({"html": html}))
            }
            other => Err(self.unsupported(other)),
        }
    }

    /// History navigation via getNavigationHistory + navigateToHistoryEntry.
    async fn history_step(
        &self,
        page: &Page,
        argv: &[String],
        delta: i64,
    ) -> Result<(), BrowserError> {
        let err = |e: chromiumoxide::error::CdpError| self.cdp_err(argv, e);
        let hist = page
            .execute(cdp_page::GetNavigationHistoryParams::default())
            .await
            .map_err(err)?;
        let idx = hist.current_index + delta;
        let entry = hist
            .entries
            .get(idx as usize)
            .ok_or_else(|| BrowserError::Failed {
                argv: argv.to_vec(),
                exit: None,
                stderr: "no further history in that direction".into(),
            })?;
        let params = cdp_page::NavigateToHistoryEntryParams::builder()
            .entry_id(entry.id)
            .build()
            .map_err(|e| self.cdp_err(argv, e))?;
        page.execute(params).await.map_err(err)?;
        Ok(())
    }

    /// `snapshot`: AX tree → versioned refs.
    async fn snapshot(
        &self,
        page: &Page,
        session: &str,
        argv: &[String],
    ) -> Result<serde_json::Value, BrowserError> {
        let err = |e: chromiumoxide::error::CdpError| self.cdp_err(argv, e);
        let params = accessibility::GetFullAxTreeParams::builder().build();
        let tree = page.execute(params).await.map_err(err)?;
        // Keep interactive roles; skip ignored/subtree-hidden nodes.
        let mut nodes: Vec<(i64, String, String)> = Vec::new();
        for n in &tree.nodes {
            if n.ignored {
                continue;
            }
            let role = n
                .role
                .as_ref()
                .and_then(|r| r.value.as_ref())
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !is_interactive_role(&role) {
                continue;
            }
            let backend_id = match n.backend_dom_node_id {
                Some(id) => *id.inner(),
                None => continue,
            };
            let name = n
                .name
                .as_ref()
                .and_then(|r| r.value.as_ref())
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            nodes.push((backend_id, role, name));
        }
        let (version, elements) = self
            .inner
            .lock()
            .await
            .sessions
            .get_mut(session)
            .map(|s| s.refs.rebuild(&nodes))
            .unwrap_or((0, vec![]));
        Ok(serde_json::json!({"version": version, "elements": elements}))
    }

    /// Resolve `argv[1]` to the live remote object id of the ref's node.
    async fn resolve_ref_object_id(
        &self,
        page: &Page,
        session: &str,
        argv: &[String],
        r: &str,
    ) -> Result<runtime::RemoteObjectId, BrowserError> {
        let err = |e: chromiumoxide::error::CdpError| self.cdp_err(argv, e);
        let target = {
            let inner = self.inner.lock().await;
            let s = inner
                .sessions
                .get(session)
                .ok_or_else(|| BrowserError::StaleRef {
                    message: "no snapshot taken yet in this session".into(),
                })?;
            s.refs.resolve(r)?.clone()
        };
        let params = dom::ResolveNodeParams::builder()
            .backend_node_id(dom::BackendNodeId::new(target.backend_node_id))
            .build();
        let resolved = page.execute(params).await.map_err(err)?;
        resolved
            .object
            .object_id
            .clone()
            .ok_or_else(|| BrowserError::Failed {
                argv: argv.to_vec(),
                exit: None,
                stderr: format!("ref {r} no longer resolves to a live node"),
            })
    }

    /// `Runtime.callFunctionOn` against a remote object id, with an
    /// optional single string argument.
    async fn call_fn_on(
        &self,
        page: &Page,
        argv: &[String],
        object_id: &runtime::RemoteObjectId,
        function: &str,
        arg: Option<String>,
    ) -> Result<(), BrowserError> {
        let err = |e: chromiumoxide::error::CdpError| self.cdp_err(argv, e);
        let mut builder = runtime::CallFunctionOnParams::builder()
            .function_declaration(function.to_string())
            .object_id(object_id.clone());
        if let Some(a) = arg {
            builder = builder.argument(
                runtime::CallArgument::builder()
                    .value(serde_json::Value::String(a))
                    .build(),
            );
        }
        let params = builder.build().map_err(|e| self.cdp_err(argv, e))?;
        page.execute(params).await.map_err(err)?;
        Ok(())
    }

    /// `wait-for --condition <enum> [--value <v>]`: canonical
    /// gsd-browser conditions (see [`super::backend`]); conditions the
    /// raw-CDP path cannot implement honestly become
    /// `UnsupportedCommand` via [`wait_plan`].
    async fn wait_for(
        &self,
        page: &Page,
        argv: &[String],
    ) -> Result<serde_json::Value, BrowserError> {
        match wait_plan(argv, &self.backend_name)? {
            WaitPlan::Delay(ms) => {
                let capped = ms.min(WAIT_MAX.as_millis() as u64);
                tokio::time::sleep(Duration::from_millis(capped)).await;
                Ok(serde_json::json!({"ok": true, "slept_ms": capped}))
            }
            WaitPlan::Probe(probe) => {
                let js = format!("(()=>{{ return {}; }})()", probe);
                let deadline = tokio::time::Instant::now() + WAIT_MAX;
                loop {
                    let hit: bool = self.eval_js(page, argv, &js).await?;
                    if hit {
                        // gsd-browser's verified shape carries "met";
                        // keep "ok" for the existing unit expectations.
                        return Ok(serde_json::json!({"ok": true, "met": true}));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(BrowserError::Failed {
                            argv: argv.to_vec(),
                            exit: None,
                            stderr: "wait-for condition not met within 30s".into(),
                        });
                    }
                    tokio::time::sleep(WAIT_POLL).await;
                }
            }
        }
    }

    /// `extract [--schema <json>] [--selector <sel>]`: structured
    /// extraction driven by `_selector`/`_attribute` hints per property,
    /// or readable text when no schema is given.
    async fn extract(
        &self,
        page: &Page,
        argv: &[String],
    ) -> Result<serde_json::Value, BrowserError> {
        let schema = flag_value(argv, "--schema");
        let selector = flag_value(argv, "--selector");
        let js = build_extract_js(schema.as_deref(), selector.as_deref());
        let v: serde_json::Value = self.eval_js(page, argv, &js).await?;
        Ok(v)
    }

    /// `screenshot --output <path> --format <png|jpeg>`.
    async fn screenshot(
        &self,
        page: &Page,
        argv: &[String],
    ) -> Result<serde_json::Value, BrowserError> {
        let err = |e: chromiumoxide::error::CdpError| self.cdp_err(argv, e);
        let path = flag_value(argv, "--output")
            .ok_or_else(|| self.unsupported("screenshot (missing --output)"))?;
        let format = match flag_value(argv, "--format").as_deref() {
            Some("jpeg") | Some("jpg") => cdp_page::CaptureScreenshotFormat::Jpeg,
            _ => cdp_page::CaptureScreenshotFormat::Png,
        };
        let params = cdp_page::CaptureScreenshotParams::builder()
            .format(format)
            .build();
        let bytes = page.screenshot(params).await.map_err(err)?;
        std::fs::write(&path, &bytes).map_err(|e| BrowserError::Failed {
            argv: argv.to_vec(),
            exit: None,
            stderr: format!("failed writing screenshot to {path}: {e}"),
        })?;
        Ok(serde_json::json!({"ok": true, "path": path, "bytes": bytes.len()}))
    }

    /// Evaluate JS and deserialize the by-value result.
    async fn eval_js<T: serde::de::DeserializeOwned>(
        &self,
        page: &Page,
        argv: &[String],
        js: &str,
    ) -> Result<T, BrowserError> {
        let err = |e: chromiumoxide::error::CdpError| self.cdp_err(argv, e);
        let params = runtime::EvaluateParams::builder()
            .expression(js.to_string())
            .return_by_value(true)
            .build()
            .map_err(|e| self.cdp_err(argv, e))?;
        let res = page.execute(params).await.map_err(err)?;
        let value = res
            .result
            .result
            .value
            .clone()
            .ok_or_else(|| BrowserError::Failed {
                argv: argv.to_vec(),
                exit: None,
                stderr: "JS evaluation returned no value".into(),
            })?;
        serde_json::from_value(value).map_err(|e| BrowserError::BadOutput {
            argv: argv.to_vec(),
            detail: format!("JS result was not the expected shape: {e}"),
        })
    }
}

/// Roles worth exposing as refs (interactive elements).
fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "checkbox"
            | "radio"
            | "combobox"
            | "listbox"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "switch"
            | "searchbox"
            | "spinbutton"
            | "slider"
    )
}

/// `argv` flag lookup: `["--flag", "value"]`.
fn flag_value(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

/// The ref argument of a `*-ref` command (`argv[1]`).
fn ref_arg(argv: &[String]) -> Result<&str, BrowserError> {
    argv.get(1).map(String::as_str).ok_or(BrowserError::Failed {
        argv: argv.to_vec(),
        exit: None,
        stderr: "ref command missing ref argument".into(),
    })
}

/// Render a Rust string as a JS string literal.
pub(crate) fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Build the extraction JS. With a schema, each property's
/// `_selector`/`_attribute` hints drive per-property scraping (a string
/// value is shorthand for `{"_selector": value}`); without one, the
/// readable text of the scope (or whole page) is returned. The schema
/// may carry gsd's `{"properties": {...}}` envelope or a bare property
/// map; both work.
fn build_extract_js(schema: Option<&str>, selector: Option<&str>) -> String {
    format!("(()=>{{{}}})()", extract_js_body(schema, selector))
}

/// The same extraction as an arrow function, for `playwright-cli eval`.
pub(crate) fn build_extract_arrow(schema: Option<&str>, selector: Option<&str>) -> String {
    format!("()=>{{{}}}", extract_js_body(schema, selector))
}

fn extract_js_body(schema: Option<&str>, selector: Option<&str>) -> String {
    let scope = match selector {
        Some(sel) => format!("document.querySelector({})", js_str(sel)),
        None => "document.body".to_string(),
    };
    match schema {
        Some(s) => {
            // The canonical `--schema` shape is gsd's `{"properties":
            // {...}}` envelope (see `normalize_extract_schema` in
            // [`super::tools`]); unwrap it so the same argv works on
            // every backend. A bare property map passes through.
            let schema_js = js_str(&unwrap_properties_envelope(s));
            format!(
                r#"
  const schema=JSON.parse({schema_js});
  const scope={scope};
  if(!scope)return{{error:"extraction scope not found"}};
  const out={{}};
  for(const [key,hint] of Object.entries(schema)){{
    const h=(typeof hint==="string")?{{_selector:hint}}:hint;
    const el=h._selector?scope.querySelector(h._selector):scope;
    if(!el){{out[key]=null;continue;}}
    out[key]=h._attribute?el.getAttribute(h._attribute):el.innerText.trim();
  }}
  return out;
"#,
                schema_js = schema_js,
                scope = scope
            )
        }
        None => format!(
            r#"
  const scope={scope};
  if(!scope)return{{error:"extraction scope not found"}};
  return{{text:scope.innerText}};
"#,
            scope = scope
        ),
    }
}

/// Unwrap a gsd-style `{"properties": {...}}` schema envelope, so the
/// canonical `--schema` shape works on the raw-CDP backends too.
fn unwrap_properties_envelope(schema: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(schema) {
        Ok(serde_json::Value::Object(mut map)) => match map.remove("properties") {
            Some(props) => props.to_string(),
            None => schema.to_string(),
        },
        _ => schema.to_string(),
    }
}

/// Parse `wait-for --condition <c> [--value <v>]`, using the canonical
/// gsd-browser condition names (see [`super::backend`]).
pub(crate) fn parse_wait_args(argv: &[String]) -> Result<(String, String), BrowserError> {
    let condition = flag_value(argv, "--condition").ok_or_else(|| BrowserError::Failed {
        argv: argv.to_vec(),
        exit: None,
        stderr: "wait-for needs --condition <selector_visible|selector_hidden|url_contains|network_idle|delay|text_visible|text_hidden|request_completed|console_message|element_count|region_stable>".into(),
    })?;
    let value = flag_value(argv, "--value").unwrap_or_default();
    Ok((condition, value))
}

/// How a canonical `wait-for` is satisfied on the raw-CDP backends
/// (native, Steel, Browserbase, Lightpanda, Playwright).
pub(crate) enum WaitPlan {
    /// Poll a JS boolean expression until it is true.
    Probe(String),
    /// Sleep once for N milliseconds, then succeed.
    Delay(u64),
}

/// Build the [`WaitPlan`] for a canonical `wait-for`.
///
/// Conditions the raw-CDP path cannot implement honestly —
/// `network_idle` (no request tracker), `request_completed`,
/// `console_message`, `element_count`, `region_stable` — become
/// [`BrowserError::UnsupportedCommand`] naming the backend, never silent
/// approximations. `selector_visible` probes *presence* (a true
/// visibility check needs per-element layout info; the approximation is
/// documented, not hidden).
pub(crate) fn wait_plan(argv: &[String], backend: &str) -> Result<WaitPlan, BrowserError> {
    let (condition, value) = parse_wait_args(argv)?;
    let v = js_str(&value);
    let probe = match condition.as_str() {
        "selector_visible" => format!("!!document.querySelector({v})"),
        "selector_hidden" => format!("!document.querySelector({v})"),
        "url_contains" => format!("location.href.includes({v})"),
        "text_visible" => {
            format!("(document.body?document.body.innerText.includes({v}):false)")
        }
        "text_hidden" => format!("!(document.body&&document.body.innerText.includes({v}))"),
        "delay" => {
            let ms: u64 = value.trim().parse().map_err(|_| BrowserError::Failed {
                argv: argv.to_vec(),
                exit: None,
                stderr: format!("wait-for delay needs --value <milliseconds>, got {value:?}"),
            })?;
            return Ok(WaitPlan::Delay(ms));
        }
        other @ ("network_idle" | "request_completed" | "console_message" | "element_count"
        | "region_stable") => {
            return Err(BrowserError::UnsupportedCommand {
                command: format!("wait-for --condition {other}"),
                backend: backend.to_string(),
            });
        }
        other => {
            return Err(BrowserError::Failed {
                argv: argv.to_vec(),
                exit: None,
                stderr: format!("unknown wait-for condition: {other}"),
            })
        }
    };
    Ok(WaitPlan::Probe(probe))
}
