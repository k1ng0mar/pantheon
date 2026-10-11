//! Versioned, typed UI protocol: structured agent output for clients.
//!
//! Pantheon is an agent, not a terminal chatbot. Its responses should be
//! able to carry the most appropriate representation of information for
//! the task: a table, a chart, a validated form, a task tracker, a diff,
//! an approval card - not just Markdown text.
//!
//! This module defines the **protocol**, not the rendering. Clients
//! (TUI, dashboard, gateway channels) render [`Component`]s with their
//! own native capabilities; the runtime never assumes a particular
//! client. Model providers never see this schema directly - the agent
//! emits UI through the `render_ui` capture tool, and the runtime
//! validates the payload before it reaches any client.
//!
//! # Envelope
//!
//! A [`UiDocument`] is the unit of structured output: a protocol
//! [`UI_PROTOCOL_VERSION`], a stable [`UiDocument::id`], and a root
//! [`UiNode`]. Nodes form a tree (see [`Layout`]); every node has a
//! stable `id` so updates and actions can address it.
//!
//! # Transport
//!
//! Documents ride the existing event ledger as
//! [`Event::UiContent`](crate::events::Event::UiContent) (and revisions as
//! [`Event::UiUpdated`](crate::events::Event::UiUpdated)), exactly like
//! `TodosUpdated` carries structured todo state. That buys persistence,
//! ordering, replay, and fan-out for free - there is no parallel UI bus.
//!
//! # Validation
//!
//! [`UiDocument::validate`] enforces the schema bounds below. Anything
//! failing validation is rejected before rendering; clients additionally
//! degrade unknown component types and newer versions via
//! [`degrade_for`] so a new protocol version never breaks an old client.
//!
//! # Actions and security
//!
//! A component may declare [`ActionRef`]s, but a component is never the
//! authority that executes an action. Actions resolve server-side:
//!
//! - [`ActionKind::Local`] never leaves the client (tab switches, expand,
//!   chart filters).
//! - [`ActionKind::Submit`] carries validated form params to the runtime.
//! - [`ActionKind::Tool`] invokes a registered tool **through the existing
//!   capability/permission gate** - the model-provided label grants
//!   nothing; the capability comes from the server-side action registry.
//! - [`ActionKind::Approval`] binds the existing approval scope
//!   (`call_id:tool:args`); grants stay one-shot and TTL-bound.
//!
//! Submissions ([`UiActionSubmission`]) are validated against the declared
//! action ([`validate_submission`]), deduplicated by `nonce`, and bound to
//! the `(ui_id, node_id, action_id)` triple. No JavaScript, HTML, shell,
//! or model-provided code is ever executed from a document.

use serde::{Deserialize, Serialize};

use crate::ident::is_slug;

/// Current protocol version. Documents with any other version fail
/// validation; clients degrade them to a fallback note via [`degrade_for`].
pub const UI_PROTOCOL_VERSION: u16 = 1;

/// The agent-facing capture tool name. The runtime intercepts calls to
/// this tool before gating (like `ask_user`): it validates the document
/// and emits `Event::UiContent`/`UiUpdated`. The registry handler itself
/// only validates, so direct executors get the same checks.
pub const RENDER_UI_TOOL_NAME: &str = "render_ui";

// --- schema bounds ---------------------------------------------------------
// Every bound is enforced by `UiDocument::validate`. They exist so one
// malicious or confused model turn cannot OOM a client or the ledger.

/// Maximum nodes in one document (root included).
pub const MAX_NODES: usize = 128;
/// Maximum nesting depth (root = 1).
pub const MAX_DEPTH: usize = 8;
/// Maximum length of titles, labels, and other short strings (chars).
pub const MAX_SHORT_LEN: usize = 200;
/// Maximum length of markdown/code bodies (chars).
pub const MAX_BODY_LEN: usize = 32_768;
/// Maximum table dimensions.
pub const MAX_TABLE_ROWS: usize = 100;
pub const MAX_TABLE_COLS: usize = 12;
/// Maximum table cell length (chars).
pub const MAX_CELL_LEN: usize = 500;
/// Maximum chart labels and series.
pub const MAX_CHART_LABELS: usize = 365;
pub const MAX_CHART_SERIES: usize = 5;
/// Maximum form fields and select options.
pub const MAX_FORM_FIELDS: usize = 30;
pub const MAX_FIELD_OPTIONS: usize = 20;
/// Maximum list / task-list items.
pub const MAX_LIST_ITEMS: usize = 100;
/// Maximum diff files and total diff lines.
pub const MAX_DIFF_FILES: usize = 5;
pub const MAX_DIFF_LINES: usize = 200;
/// Maximum serialized bytes of a `Custom` payload.
pub const MAX_CUSTOM_BYTES: usize = 16_384;
/// Maximum action params entries per submission.
pub const MAX_ACTION_PARAMS: usize = 30;

/// Typed protocol errors. Every variant is renderable as a client-facing
/// note - validation failures never panic the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiError {
    /// `version` is not [`UI_PROTOCOL_VERSION`].
    UnsupportedVersion { got: u16 },
    /// Unknown component `type` tag.
    UnknownComponent { tag: String },
    /// Tree deeper than [`MAX_DEPTH`].
    TooDeep { depth: usize },
    /// More nodes than [`MAX_NODES`].
    TooManyNodes { count: usize },
    /// A string field exceeds its bound.
    TooLong { field: &'static str, max: usize },
    /// Too many rows/items/fields in a bounded collection.
    TooMany { field: &'static str, max: usize },
    /// An id is not slug-shaped ([`is_slug`]).
    BadId { field: &'static str, id: String },
    /// Chart series length does not match labels, or values are non-finite.
    BadChartData { reason: &'static str },
    /// A form field definition is invalid.
    BadField { name: String, reason: &'static str },
    /// Submission references an unknown ui/node/action.
    UnknownAction { what: &'static str, id: String },
    /// Submission params fail the declared schema.
    BadParams { reason: String },
    /// Empty or already-seen nonce.
    BadNonce,
    /// `Custom` payload exceeds [`MAX_CUSTOM_BYTES`].
    CustomTooLarge { bytes: usize },
    /// The JSON is not a well-formed document at all.
    Malformed { reason: String },
}

impl std::fmt::Display for UiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiError::UnsupportedVersion { got } => write!(
                f,
                "unsupported UI protocol version {got} (this client speaks {UI_PROTOCOL_VERSION})"
            ),
            UiError::UnknownComponent { tag } => {
                write!(f, "unknown UI component type {tag:?}")
            }
            UiError::TooDeep { depth } => write!(f, "UI tree too deep ({depth})"),
            UiError::TooManyNodes { count } => {
                write!(f, "UI document has too many nodes ({count})")
            }
            UiError::TooLong { field, max } => {
                write!(f, "UI field {field} exceeds {max} chars")
            }
            UiError::TooMany { field, max } => {
                write!(f, "UI field {field} exceeds {max} entries")
            }
            UiError::BadId { field, id } => {
                write!(f, "UI {field} id {id:?} is not slug-shaped")
            }
            UiError::BadChartData { reason } => write!(f, "bad chart data: {reason}"),
            UiError::BadField { name, reason } => {
                write!(f, "bad form field {name:?}: {reason}")
            }
            UiError::UnknownAction { what, id } => {
                write!(f, "unknown UI {what} {id:?}")
            }
            UiError::BadParams { reason } => write!(f, "bad UI action params: {reason}"),
            UiError::BadNonce => write!(f, "bad or replayed UI action nonce"),
            UiError::CustomTooLarge { bytes } => {
                write!(f, "custom UI payload too large ({bytes} bytes)")
            }
            UiError::Malformed { reason } => write!(f, "malformed UI document: {reason}"),
        }
    }
}

impl std::error::Error for UiError {}

/// One structured response from the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiDocument {
    /// Must equal [`UI_PROTOCOL_VERSION`].
    pub version: u16,
    /// Stable id, slug-shaped. Convention: `{turn_id}-ui_{i}`, mirroring
    /// tool call ids, so actions and revisions address exactly one
    /// document.
    pub id: String,
    /// The component tree.
    pub root: UiNode,
}

/// One addressable node in a [`UiDocument`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiNode {
    /// Stable within the document, slug-shaped. Actions and revisions
    /// address nodes by this id.
    pub id: String,
    #[serde(flatten)]
    pub component: Component,
}

fn check_id(field: &'static str, id: &str) -> Result<(), UiError> {
    if is_slug(id) {
        Ok(())
    } else {
        Err(UiError::BadId {
            field,
            id: id.to_string(),
        })
    }
}

fn check_len(field: &'static str, s: &str, max: usize) -> Result<(), UiError> {
    if s.chars().count() <= max {
        Ok(())
    } else {
        Err(UiError::TooLong { field, max })
    }
}

/// A renderable component. Externally tagged (`{"type": "table", ...}`) so
/// clients can match on the tag without parsing the whole payload, and so
/// unknown tags are detectable for graceful degradation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Component {
    Markdown(Markdown),
    Table(Table),
    Chart(Chart),
    Metrics(Metrics),
    List(List),
    Code(Code),
    Refs(Refs),
    Form(Form),
    TaskList(TaskList),
    Progress(Progress),
    Details(Details),
    Diff(Diff),
    Approval(ApprovalCard),
    Layout(Layout),
    Custom(Custom),
}

impl Component {
    /// The wire tag for this component (`table`, `chart`, ...).
    pub fn tag(&self) -> &'static str {
        match self {
            Component::Markdown(_) => "markdown",
            Component::Table(_) => "table",
            Component::Chart(_) => "chart",
            Component::Metrics(_) => "metrics",
            Component::List(_) => "list",
            Component::Code(_) => "code",
            Component::Refs(_) => "refs",
            Component::Form(_) => "form",
            Component::TaskList(_) => "task_list",
            Component::Progress(_) => "progress",
            Component::Details(_) => "details",
            Component::Diff(_) => "diff",
            Component::Approval(_) => "approval",
            Component::Layout(_) => "layout",
            Component::Custom(_) => "custom",
        }
    }

    /// Validate this component's own fields (not children - see
    /// [`UiDocument::validate`] for the tree walk).
    fn validate_self(&self) -> Result<(), UiError> {
        match self {
            Component::Markdown(m) => {
                check_len("markdown.text", &m.text, MAX_BODY_LEN)?;
                if let Some(t) = &m.title {
                    check_len("markdown.title", t, MAX_SHORT_LEN)?;
                }
            }
            Component::Table(t) => {
                if t.headers.len() > MAX_TABLE_COLS {
                    return Err(UiError::TooMany {
                        field: "table.headers",
                        max: MAX_TABLE_COLS,
                    });
                }
                if t.rows.len() > MAX_TABLE_ROWS {
                    return Err(UiError::TooMany {
                        field: "table.rows",
                        max: MAX_TABLE_ROWS,
                    });
                }
                for h in &t.headers {
                    check_len("table.header", h, MAX_CELL_LEN)?;
                }
                for (i, row) in t.rows.iter().enumerate() {
                    if row.len() > MAX_TABLE_COLS {
                        return Err(UiError::TooMany {
                            field: "table.columns",
                            max: MAX_TABLE_COLS,
                        });
                    }
                    for cell in row {
                        check_len("table.cell", cell, MAX_CELL_LEN)?;
                    }
                    let _ = i;
                }
                if let Some(a) = &t.align {
                    if a.len() > MAX_TABLE_COLS {
                        return Err(UiError::TooMany {
                            field: "table.align",
                            max: MAX_TABLE_COLS,
                        });
                    }
                }
            }
            Component::Chart(c) => c.validate()?,
            Component::Metrics(m) => {
                if m.items.len() > MAX_LIST_ITEMS {
                    return Err(UiError::TooMany {
                        field: "metrics.items",
                        max: MAX_LIST_ITEMS,
                    });
                }
                for item in &m.items {
                    check_len("metrics.label", &item.label, MAX_SHORT_LEN)?;
                    check_len("metrics.value", &item.value, MAX_SHORT_LEN)?;
                    if let Some(d) = &item.delta {
                        check_len("metrics.delta", d, MAX_SHORT_LEN)?;
                    }
                }
                if let Some(t) = &m.title {
                    check_len("metrics.title", t, MAX_SHORT_LEN)?;
                }
            }
            Component::List(l) => {
                if l.items.len() > MAX_LIST_ITEMS {
                    return Err(UiError::TooMany {
                        field: "list.items",
                        max: MAX_LIST_ITEMS,
                    });
                }
                for item in &l.items {
                    check_len("list.item", &item.text, MAX_CELL_LEN)?;
                }
                if let Some(t) = &l.title {
                    check_len("list.title", t, MAX_SHORT_LEN)?;
                }
            }
            Component::Code(c) => {
                check_len("code.code", &c.code, MAX_BODY_LEN)?;
                check_len("code.language", &c.language, 40)?;
                if let Some(t) = &c.title {
                    check_len("code.title", t, MAX_SHORT_LEN)?;
                }
            }
            Component::Refs(r) => {
                if r.items.len() > MAX_LIST_ITEMS {
                    return Err(UiError::TooMany {
                        field: "refs.items",
                        max: MAX_LIST_ITEMS,
                    });
                }
                for item in &r.items {
                    check_len("refs.title", &item.title, MAX_SHORT_LEN)?;
                    check_len("refs.url", &item.url, 2048)?;
                    if let Some(n) = &item.note {
                        check_len("refs.note", n, MAX_CELL_LEN)?;
                    }
                }
            }
            Component::Form(f) => f.validate()?,
            Component::TaskList(t) => {
                if t.items.len() > MAX_LIST_ITEMS {
                    return Err(UiError::TooMany {
                        field: "task_list.items",
                        max: MAX_LIST_ITEMS,
                    });
                }
                for item in &t.items {
                    check_id("task_list.item", &item.id)?;
                    check_len("task_list.label", &item.label, MAX_CELL_LEN)?;
                }
                if let Some(t) = &t.title {
                    check_len("task_list.title", t, MAX_SHORT_LEN)?;
                }
            }
            Component::Progress(p) => {
                check_len("progress.label", &p.label, MAX_SHORT_LEN)?;
            }
            Component::Details(d) => {
                check_len("details.summary", &d.summary, MAX_SHORT_LEN)?;
                d.body.validate_tree(2)?;
            }
            Component::Diff(d) => d.validate()?,
            Component::Approval(a) => {
                check_len("approval.title", &a.title, MAX_SHORT_LEN)?;
                check_len("approval.detail", &a.detail, MAX_BODY_LEN)?;
                a.approve.validate()?;
                a.deny.validate()?;
            }
            Component::Layout(l) => {
                // Children are validated by the document tree walk
                // (depth + node budget); nothing extra here.
                let _ = l;
            }
            Component::Custom(c) => {
                check_len("custom.name", &c.name, MAX_SHORT_LEN)?;
                let bytes = serde_json::to_vec(&c.payload)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX);
                if bytes > MAX_CUSTOM_BYTES {
                    return Err(UiError::CustomTooLarge { bytes });
                }
                c.fallback.validate_tree(2)?;
            }
        }
        Ok(())
    }
}

// --- Tier 1: core output ---------------------------------------------------

/// Rich text. The default component - anything renderable as Markdown
/// should stay Markdown rather than reaching for a fancier type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Markdown {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A data table with headers and aligned columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Per-column alignment; defaults to left. Shorter than headers is
    /// fine (missing entries are left-aligned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Vec<Align>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    Left,
    Right,
    Center,
}

/// A data visualization. The TUI renders these as Unicode/ASCII charts;
/// graphical clients render them natively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chart {
    pub chart_type: ChartType,
    pub labels: Vec<String>,
    pub series: Vec<Series>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChartType {
    Line,
    Bar,
    Pie,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub name: String,
    pub values: Vec<f64>,
}

impl Chart {
    fn validate(&self) -> Result<(), UiError> {
        if self.labels.len() > MAX_CHART_LABELS {
            return Err(UiError::TooMany {
                field: "chart.labels",
                max: MAX_CHART_LABELS,
            });
        }
        if self.series.len() > MAX_CHART_SERIES {
            return Err(UiError::TooMany {
                field: "chart.series",
                max: MAX_CHART_SERIES,
            });
        }
        for label in &self.labels {
            check_len("chart.label", label, MAX_SHORT_LEN)?;
        }
        for s in &self.series {
            check_len("chart.series_name", &s.name, MAX_SHORT_LEN)?;
            if s.values.len() != self.labels.len() {
                return Err(UiError::BadChartData {
                    reason: "series length does not match labels",
                });
            }
            if s.values.iter().any(|v| !v.is_finite()) {
                return Err(UiError::BadChartData {
                    reason: "series contains non-finite value",
                });
            }
        }
        if let Some(t) = &self.title {
            check_len("chart.title", t, MAX_SHORT_LEN)?;
        }
        Ok(())
    }
}

/// Key-value summaries and headline metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub items: Vec<Metric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub label: String,
    pub value: String,
    /// e.g. "+12%". Display-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
}

/// A bulleted/numbered list with optional per-item status markers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct List {
    pub items: Vec<ListItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListItem {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Error,
    Info,
    Pending,
}

/// A code block with an optional language hint for syntax-aware clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Code {
    pub code: String,
    #[serde(default)]
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Source references: links with titles and optional notes. Rendered as
/// a reference list; clients may make them tappable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refs {
    pub items: Vec<RefItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefItem {
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// --- Tier 2: interactive output --------------------------------------------

/// A validated form. Editing fields is client-local; nothing reaches the
/// runtime until the user submits, at which point the params are
/// validated against the field definitions by [`validate_submission`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Form {
    pub fields: Vec<FormField>,
    pub submit: ActionRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    /// Slug-shaped; becomes the params key on submit.
    pub name: String,
    pub label: String,
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldType {
    Text,
    LongText,
    Number,
    Boolean,
    /// Masked input (passwords, tokens). Rendered with a reveal toggle
    /// where the client supports it.
    Secret,
    Select {
        options: Vec<String>,
    },
}

impl Form {
    fn validate(&self) -> Result<(), UiError> {
        if self.fields.len() > MAX_FORM_FIELDS {
            return Err(UiError::TooMany {
                field: "form.fields",
                max: MAX_FORM_FIELDS,
            });
        }
        if self.fields.is_empty() {
            return Err(UiError::BadField {
                name: String::new(),
                reason: "form has no fields",
            });
        }
        for f in &self.fields {
            check_id("form.field", &f.name)?;
            check_len("form.label", &f.label, MAX_SHORT_LEN)?;
            if let Some(d) = &f.default {
                check_len("form.default", d, MAX_CELL_LEN)?;
            }
            if let Some(p) = &f.placeholder {
                check_len("form.placeholder", p, MAX_SHORT_LEN)?;
            }
            match &f.field_type {
                FieldType::Select { options } => {
                    if options.is_empty() || options.len() > MAX_FIELD_OPTIONS {
                        return Err(UiError::BadField {
                            name: f.name.clone(),
                            reason: "select needs 1..=20 options",
                        });
                    }
                    for o in options {
                        check_len("form.option", o, MAX_SHORT_LEN)?;
                    }
                    if let Some(d) = &f.default {
                        if !options.iter().any(|o| o == d) {
                            return Err(UiError::BadField {
                                name: f.name.clone(),
                                reason: "default is not one of the options",
                            });
                        }
                    }
                }
                FieldType::Number => {
                    if let Some(d) = &f.default {
                        if d.trim().parse::<f64>().is_err() {
                            return Err(UiError::BadField {
                                name: f.name.clone(),
                                reason: "default is not a number",
                            });
                        }
                    }
                }
                FieldType::Boolean => {
                    if let Some(d) = &f.default {
                        if !matches!(d.as_str(), "true" | "false") {
                            return Err(UiError::BadField {
                                name: f.name.clone(),
                                reason: "default is not true/false",
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        // No two fields may share a name: params would be ambiguous.
        let mut seen = std::collections::HashSet::new();
        for f in &self.fields {
            if !seen.insert(&f.name) {
                return Err(UiError::BadField {
                    name: f.name.clone(),
                    reason: "duplicate field name",
                });
            }
        }
        self.submit.validate()?;
        if let Some(t) = &self.title {
            check_len("form.title", t, MAX_SHORT_LEN)?;
        }
        if let Some(d) = &self.description {
            check_len("form.description", d, MAX_BODY_LEN)?;
        }
        Ok(())
    }
}

/// A checklist with per-item state. Live revisions arrive as
/// `Event::UiUpdated` with the same document id and a bumped revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskList {
    pub items: Vec<TaskItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskItem {
    pub id: String,
    pub label: String,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

/// A determinate progress indicator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub label: String,
    pub done: u64,
    pub total: u64,
}

/// An expandable section: the summary is always visible, the body is
/// behind user expansion. Clients without expansion render both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Details {
    pub summary: String,
    pub body: Box<UiNode>,
}

/// A structured unified diff. Line data mirrors the TUI's inline diff
/// renderer so both speak the same shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub files: Vec<DiffFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
}

impl Diff {
    fn validate(&self) -> Result<(), UiError> {
        if self.files.len() > MAX_DIFF_FILES {
            return Err(UiError::TooMany {
                field: "diff.files",
                max: MAX_DIFF_FILES,
            });
        }
        let mut lines = 0usize;
        for f in &self.files {
            check_len("diff.path", &f.path, MAX_CELL_LEN)?;
            for h in &f.hunks {
                check_len("diff.header", &h.header, MAX_SHORT_LEN)?;
                for l in &h.lines {
                    check_len("diff.line", &l.text, MAX_CELL_LEN)?;
                    lines += 1;
                }
            }
        }
        if lines > MAX_DIFF_LINES {
            return Err(UiError::TooMany {
                field: "diff.lines",
                max: MAX_DIFF_LINES,
            });
        }
        if let Some(t) = &self.title {
            check_len("diff.title", t, MAX_SHORT_LEN)?;
        }
        Ok(())
    }
}

/// A review interface for a consequential action. This does NOT create a
/// new approval mechanism: `approve`/`deny` bind the existing approval
/// scope (see [`ActionKind::Approval`]), and the card renders the same
/// decision the runtime would otherwise ask about inline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCard {
    pub title: String,
    pub detail: String,
    pub approve: ActionRef,
    pub deny: ActionRef,
    /// Human-readable hint of what is being decided (e.g. the tool name).
    /// Display-only; never authority.
    #[serde(default)]
    pub scope_hint: String,
}

// --- actions -----------------------------------------------------------------

/// A declared interaction on a component. The `label` is display-only: it
/// never determines permissions. Authority comes from the server-side
/// action registry, keyed by `(ui_id, node_id, action_id)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionRef {
    pub action_id: String,
    pub label: String,
    pub kind: ActionKind,
}

/// What happens when the action fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionKind {
    /// Client-local: tab switches, expand/collapse, chart filters. Never
    /// leaves the client, never touches the runtime.
    Local,
    /// Submit validated form params to a tool. The tool name is resolved
    /// against the server-side registry - a document cannot invent tools -
    /// and the call runs through the normal capability gate.
    Submit { tool: String },
    /// Invoke a tool with merged params (submission params over
    /// `fixed_params`). Same registry + gate as [`ActionKind::Submit`].
    Tool {
        tool: String,
        /// Fixed params merged under the submission params.
        #[serde(default)]
        fixed_params: serde_json::Map<String, serde_json::Value>,
    },
    /// Resolve an approval. Binds the existing approval scope; the grant
    /// stays one-shot and TTL-bound exactly like any other approval.
    Approval { decision: ApprovalDecision },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Grant,
    Deny,
}

impl ActionRef {
    fn validate(&self) -> Result<(), UiError> {
        check_id("action", &self.action_id)?;
        check_len("action.label", &self.label, MAX_SHORT_LEN)?;
        match &self.kind {
            ActionKind::Tool { tool, fixed_params } => {
                // Slug-shaped so it can only name a registered tool id,
                // never a path or command line.
                check_id("action.tool", tool)?;
                if fixed_params.len() > MAX_ACTION_PARAMS {
                    return Err(UiError::TooMany {
                        field: "action.fixed_params",
                        max: MAX_ACTION_PARAMS,
                    });
                }
            }
            ActionKind::Submit { tool } => {
                check_id("action.tool", tool)?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// What the client sends back when the user triggers an action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiActionSubmission {
    pub ui_id: String,
    /// The document revision the client rendered. The runtime rejects
    /// submissions against a superseded revision: the user must see
    /// what they are acting on (prevents a revised action definition
    /// from executing under the old display).
    pub revision: u64,
    pub node_id: String,
    pub action_id: String,
    pub params: serde_json::Map<String, serde_json::Value>,
    /// Client-generated nonce; the runtime rejects replays.
    pub nonce: String,
}

// --- Tier 3: composition ---------------------------------------------------
// These give the architecture a path to richer composition without
// requiring every client to implement a plugin system on day one.

/// Composable layout. Clients that cannot do a layout degrade to a
/// vertical stack of the children (see [`degrade_for`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub kind: LayoutKind,
    pub children: Vec<UiNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutKind {
    /// Vertical sections.
    Stack,
    /// Side-by-side where the client supports it, stacked otherwise.
    Columns,
    /// Tabbed views; tab switching is [`ActionKind::Local`].
    Tabs,
}

/// An extensible component. Unknown `name`s are never rendered blindly:
/// the client renders [`Custom::fallback`] unless it has explicitly
/// registered a renderer for `(name, version)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Custom {
    pub name: String,
    pub version: u16,
    pub payload: serde_json::Value,
    pub fallback: Box<UiNode>,
}

// --- document validation ---------------------------------------------------

impl UiDocument {
    /// Validate the whole document: version, ids, tree depth, node
    /// budget, and every component's own fields.
    pub fn validate(&self) -> Result<(), UiError> {
        if self.version != UI_PROTOCOL_VERSION {
            return Err(UiError::UnsupportedVersion { got: self.version });
        }
        check_id("document", &self.id)?;
        let count = self.root.count_nodes();
        if count > MAX_NODES {
            return Err(UiError::TooManyNodes { count });
        }
        self.root.validate_tree(1)
    }

    /// Parse from a JSON value with graceful handling of unknown
    /// component tags: unknown subtrees are replaced with a Markdown
    /// fallback note, and the unknown tags are reported so the caller can
    /// log or surface them. Malformed JSON still fails.
    pub fn from_value_lenient(value: &serde_json::Value) -> Result<(Self, Vec<String>), UiError> {
        let mut unknown = Vec::new();
        let mut scrubbed = value.clone();
        scrub_unknown_tags(&mut scrubbed, &mut unknown);
        let doc: UiDocument = serde_json::from_value(scrubbed).map_err(|e| UiError::Malformed {
            reason: e.to_string(),
        })?;
        Ok((doc, unknown))
    }
}

impl UiNode {
    fn count_nodes(&self) -> usize {
        1 + match &self.component {
            Component::Details(d) => d.body.count_nodes(),
            Component::Layout(l) => l.children.iter().map(UiNode::count_nodes).sum(),
            Component::Custom(c) => c.fallback.count_nodes(),
            _ => 0,
        }
    }

    fn validate_tree(&self, depth: usize) -> Result<(), UiError> {
        if depth > MAX_DEPTH {
            return Err(UiError::TooDeep { depth });
        }
        check_id("node", &self.id)?;
        self.component.validate_self()?;
        match &self.component {
            Component::Details(d) => d.body.validate_tree(depth + 1)?,
            Component::Layout(l) => {
                for child in &l.children {
                    child.validate_tree(depth + 1)?;
                }
            }
            Component::Custom(c) => c.fallback.validate_tree(depth + 1)?,
            _ => {}
        }
        Ok(())
    }

    /// Find a node by id (depth-first).
    pub fn find(&self, id: &str) -> Option<&UiNode> {
        if self.id == id {
            return Some(self);
        }
        match &self.component {
            Component::Details(d) => d.body.find(id),
            Component::Layout(l) => l.children.iter().find_map(|c| c.find(id)),
            Component::Custom(c) => c.fallback.find(id),
            _ => None,
        }
    }
}

impl UiDocument {
    /// Find a declared action by node id + action id. Used by the
    /// runtime to resolve submissions against the published document
    /// (the document is the action registry - no parallel state).
    pub fn find_action(&self, node_id: &str, action_id: &str) -> Option<&ActionRef> {
        self.root
            .find(node_id)
            .and_then(|n| find_action(n, action_id))
    }

    /// Collect every declared action in the document, for the runtime to
    /// index at publish time.
    pub fn actions(&self) -> Vec<(&str, &ActionRef)> {
        let mut out = Vec::new();
        collect_actions(&self.root, &mut out);
        out
    }

    /// Does the document contain an approval card? The runtime parks the
    /// turn when one is published, like `ask_user`.
    pub fn has_approval_card(&self) -> bool {
        fn walk(node: &UiNode) -> bool {
            if matches!(node.component, Component::Approval(_)) {
                return true;
            }
            match &node.component {
                Component::Details(d) => walk(&d.body),
                Component::Layout(l) => l.children.iter().any(walk),
                Component::Custom(c) => walk(&c.fallback),
                _ => false,
            }
        }
        walk(&self.root)
    }
}

fn collect_actions<'a>(node: &'a UiNode, out: &mut Vec<(&'a str, &'a ActionRef)>) {
    match &node.component {
        Component::Form(f) => out.push((node.id.as_str(), &f.submit)),
        Component::Approval(a) => {
            out.push((node.id.as_str(), &a.approve));
            out.push((node.id.as_str(), &a.deny));
        }
        Component::Details(d) => collect_actions(&d.body, out),
        Component::Layout(l) => {
            for child in &l.children {
                collect_actions(child, out);
            }
        }
        Component::Custom(c) => collect_actions(&c.fallback, out),
        _ => {}
    }
}

/// Walk a raw JSON value, replacing any node whose component `type` tag
/// the protocol does not know with a Markdown fallback note. Unknown tags
/// are collected for the caller.
fn scrub_unknown_tags(value: &mut serde_json::Value, out: &mut Vec<String>) {
    const KNOWN: &[&str] = &[
        "markdown",
        "table",
        "chart",
        "metrics",
        "list",
        "code",
        "refs",
        "form",
        "task_list",
        "progress",
        "details",
        "diff",
        "approval",
        "layout",
        "custom",
    ];
    match value {
        serde_json::Value::Object(map) => {
            // Only UiNode objects are components: they carry both "id"
            // and the flattened "type" tag. Other internally-tagged
            // enums (FieldType, chart kinds) share the "type" key but
            // have no "id" and must not be scrubbed.
            let tag = map.get("type").and_then(|v| v.as_str()).map(str::to_string);
            let is_node = map.get("id").and_then(|v| v.as_str()).is_some();
            if let Some(tag) = tag {
                if is_node && !KNOWN.contains(&tag.as_str()) {
                    if !out.contains(&tag) {
                        out.push(tag.clone());
                    }
                    let id = map.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                    *value = serde_json::json!({
                        "id": format!("{id}-unsupported"),
                        "type": "markdown",
                        "text": format!("(unsupported component: {tag})"),
                    });
                    return;
                }
            }
            for v in map.values_mut() {
                scrub_unknown_tags(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                scrub_unknown_tags(v, out);
            }
        }
        _ => {}
    }
}

// --- submission validation ---------------------------------------------------

impl UiActionSubmission {
    /// Validate a client submission against the declared document:
    /// ids resolve, the action is declared on the node, params match the
    /// form schema (for [`ActionKind::Submit`]), and the nonce is
    /// well-formed. Replay protection (nonce already seen) is the
    /// runtime's job - it owns the seen-set.
    pub fn validate_against(&self, doc: &UiDocument) -> Result<(), UiError> {
        check_id("submission.ui", &self.ui_id)?;
        check_id("submission.node", &self.node_id)?;
        check_id("submission.action", &self.action_id)?;
        if self.ui_id != doc.id {
            return Err(UiError::UnknownAction {
                what: "document",
                id: self.ui_id.clone(),
            });
        }
        if self.nonce.is_empty() || self.nonce.len() > 128 {
            return Err(UiError::BadNonce);
        }
        if self.params.len() > MAX_ACTION_PARAMS {
            return Err(UiError::TooMany {
                field: "action.params",
                max: MAX_ACTION_PARAMS,
            });
        }
        let node = doc.root.find(&self.node_id).ok_or(UiError::UnknownAction {
            what: "node",
            id: self.node_id.clone(),
        })?;
        let action = find_action(node, &self.action_id).ok_or(UiError::UnknownAction {
            what: "action",
            id: self.action_id.clone(),
        })?;
        match &action.kind {
            ActionKind::Local => Err(UiError::BadParams {
                reason: "local actions never reach the runtime".to_string(),
            }),
            ActionKind::Submit { .. } => {
                let form = match &node.component {
                    Component::Form(f) => f,
                    _ => {
                        return Err(UiError::BadParams {
                            reason: "submit action on a non-form node".to_string(),
                        })
                    }
                };
                validate_form_params(form, &self.params)
            }
            ActionKind::Tool { .. } => {
                // A tool action on a form's submit button carries the
                // form's values: validate them against the form schema
                // before they merge into tool args. Standalone tool
                // buttons have no form schema; the tool validates its
                // own args.
                if let Component::Form(f) = &node.component {
                    validate_form_params(f, &self.params)?;
                }
                Ok(())
            }
            ActionKind::Approval { .. } => {
                // Params ride through to the approval path. Here we only
                // enforce the entry-count bound (done above).
                Ok(())
            }
        }
    }
}

/// Find a declared action on a node by id.
fn find_action<'a>(node: &'a UiNode, action_id: &str) -> Option<&'a ActionRef> {
    match &node.component {
        Component::Form(f) if f.submit.action_id == action_id => Some(&f.submit),
        Component::Approval(a) => {
            if a.approve.action_id == action_id {
                Some(&a.approve)
            } else if a.deny.action_id == action_id {
                Some(&a.deny)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn validate_form_params(
    form: &Form,
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), UiError> {
    for field in &form.fields {
        match params.get(&field.name) {
            None => {
                if field.required {
                    return Err(UiError::BadParams {
                        reason: format!("missing required field {:?}", field.name),
                    });
                }
            }
            Some(v) => validate_field_value(field, v)?,
        }
    }
    for key in params.keys() {
        if !form.fields.iter().any(|f| &f.name == key) {
            return Err(UiError::BadParams {
                reason: format!("unknown field {key:?}"),
            });
        }
    }
    Ok(())
}

fn validate_field_value(field: &FormField, v: &serde_json::Value) -> Result<(), UiError> {
    let bad = |reason: &'static str| UiError::BadParams {
        reason: format!("field {:?}: {reason}", field.name),
    };
    match &field.field_type {
        FieldType::Text | FieldType::LongText | FieldType::Secret => {
            let s = v.as_str().ok_or(bad("expected a string"))?;
            if s.chars().count() > MAX_CELL_LEN {
                return Err(bad("value too long"));
            }
        }
        FieldType::Number => {
            let ok =
                v.as_f64().is_some() || v.as_str().is_some_and(|s| s.trim().parse::<f64>().is_ok());
            if !ok {
                return Err(bad("expected a number"));
            }
        }
        FieldType::Boolean => {
            let ok =
                v.as_bool().is_some() || v.as_str().is_some_and(|s| matches!(s, "true" | "false"));
            if !ok {
                return Err(bad("expected true/false"));
            }
        }
        FieldType::Select { options } => {
            let s = v.as_str().ok_or(bad("expected a string"))?;
            if !options.iter().any(|o| o == s) {
                return Err(bad("not one of the options"));
            }
        }
    }
    Ok(())
}

// --- client capabilities and graceful degradation ---------------------------

/// What a client can render. Clients advertise this (or the runtime knows
/// it statically per client); [`degrade_for`] rewrites a document so it
/// only contains supported components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCaps {
    /// Component `type` tags the client renders natively.
    pub supported: Vec<String>,
    /// Highest protocol version the client speaks.
    pub max_version: u16,
}

impl ClientCaps {
    /// A client that renders everything in the v1 catalogue.
    pub fn full() -> Self {
        Self {
            supported: vec![
                "markdown".to_string(),
                "table".to_string(),
                "chart".to_string(),
                "metrics".to_string(),
                "list".to_string(),
                "code".to_string(),
                "refs".to_string(),
                "form".to_string(),
                "task_list".to_string(),
                "progress".to_string(),
                "details".to_string(),
                "diff".to_string(),
                "approval".to_string(),
                "layout".to_string(),
                "custom".to_string(),
            ],
            max_version: UI_PROTOCOL_VERSION,
        }
    }

    /// A minimal client: Markdown plus text fallbacks only.
    pub fn minimal() -> Self {
        Self {
            supported: vec!["markdown".to_string()],
            max_version: UI_PROTOCOL_VERSION,
        }
    }

    pub fn supports(&self, tag: &str) -> bool {
        self.supported.iter().any(|s| s == tag)
    }
}

/// Rewrite a document so every node is renderable under `caps`.
/// Unsupported components degrade along a fixed chain (chart → table,
/// form → field list, details → summary + body, layout → stacked
/// children, custom → declared fallback); a document whose version is
/// newer than the client becomes a single explanatory note.
pub fn degrade_for(doc: &UiDocument, caps: &ClientCaps) -> UiDocument {
    if doc.version > caps.max_version {
        return UiDocument {
            version: UI_PROTOCOL_VERSION,
            id: doc.id.clone(),
            root: UiNode {
                id: format!("{}-fallback", doc.id),
                component: Component::Markdown(Markdown {
                    title: None,
                    text: format!(
                        "This content needs UI protocol v{} (this client speaks v{}).",
                        doc.version, caps.max_version
                    ),
                }),
            },
        };
    }
    let mut out = doc.clone();
    degrade_node(&mut out.root, caps);
    out
}

fn degrade_node(node: &mut UiNode, caps: &ClientCaps) {
    // Recurse first so children are already degraded.
    match &mut node.component {
        Component::Details(d) => degrade_node(&mut d.body, caps),
        Component::Layout(l) => {
            for child in &mut l.children {
                degrade_node(child, caps);
            }
        }
        Component::Custom(c) => degrade_node(&mut c.fallback, caps),
        _ => {}
    }
    let tag = node.component.tag();
    if caps.supports(tag) {
        // Custom still needs a registered renderer for its (name, version);
        // without one the declared fallback is the honest render.
        if tag == "custom" {
            let mut node_id = node.id.clone();
            std::mem::swap(&mut node_id, &mut node.id);
            if let Component::Custom(c) = &node.component {
                let mut fallback = c.fallback.as_ref().clone();
                fallback.id = node_id;
                node.component = fallback.component;
            }
        }
        return;
    }
    let replacement: Component = match &node.component {
        Component::Chart(c) => {
            let mut headers = vec!["".to_string()];
            headers.extend(c.labels.iter().cloned());
            let rows = c
                .series
                .iter()
                .map(|s| {
                    let mut row = vec![s.name.clone()];
                    row.extend(s.values.iter().map(|v| format!("{v}")));
                    row
                })
                .collect();
            Component::Table(Table {
                headers,
                rows,
                align: None,
                title: c.title.clone(),
            })
        }
        Component::Form(f) => {
            let mut text = String::new();
            if let Some(d) = &f.description {
                text.push_str(d);
                text.push_str("\n\n");
            }
            for field in &f.fields {
                text.push_str(&format!(
                    "- **{}** ({}): {}\n",
                    field.label,
                    field.name,
                    match &field.field_type {
                        FieldType::Select { options } =>
                            format!("one of: {}", options.join(", ")),
                        t => format!("{t:?}").to_lowercase(),
                    }
                ));
            }
            text.push_str("\n*This client cannot render interactive forms.*");
            Component::Markdown(Markdown {
                title: f.title.clone(),
                text,
            })
        }
        Component::TaskList(t) => Component::List(List {
            title: t.title.clone(),
            items: t
                .items
                .iter()
                .map(|i| ListItem {
                    text: format!(
                        "[{}] {}",
                        match i.state {
                            TaskState::Done => "x",
                            TaskState::Running => ">",
                            TaskState::Failed => "!",
                            TaskState::Skipped => "-",
                            TaskState::Pending => " ",
                        },
                        i.label
                    ),
                    status: None,
                })
                .collect(),
        }),
        Component::Progress(p) => {
            let pct = p
                .done
                .checked_mul(100)
                .and_then(|v| v.checked_div(p.total))
                .unwrap_or(0)
                .min(100);
            Component::Markdown(Markdown {
                title: None,
                text: format!("{}: {}% ({}/{})", p.label, pct, p.done, p.total),
            })
        }
        Component::Details(d) => {
            let body_text = format!(
                "{}\n\n{}",
                d.summary,
                node_text_fallback(&d.body.component)
            );
            Component::Markdown(Markdown {
                title: None,
                text: body_text,
            })
        }
        Component::Diff(d) => {
            let mut text = String::new();
            for f in &d.files {
                text.push_str(&format!("--- {}\n", f.path));
                for h in &f.hunks {
                    for l in &h.lines {
                        let sigil = match l.kind {
                            DiffLineKind::Add => "+",
                            DiffLineKind::Del => "-",
                            DiffLineKind::Context => " ",
                        };
                        text.push_str(&format!("{sigil}{}\n", l.text));
                    }
                }
            }
            Component::Code(Code {
                title: d.title.clone(),
                language: "diff".to_string(),
                code: text,
            })
        }
        Component::Approval(a) => Component::Markdown(Markdown {
            title: Some(a.title.clone()),
            text: format!(
                "{}\n\n**Approve:** {}\n**Deny:** {}\n\n*Use your client's approval controls to decide.*",
                a.detail, a.approve.label, a.deny.label
            ),
        }),
        Component::Layout(l) => {
            // Flatten: stack the (already degraded) children as sections.
            let mut text = String::new();
            for child in &l.children {
                let t = node_text_fallback(&child.component);
                if !t.is_empty() {
                    text.push_str(&t);
                    text.push_str("\n\n");
                }
            }
            Component::Markdown(Markdown {
                title: None,
                text: text.trim_end().to_string(),
            })
        }
        _ => Component::Markdown(Markdown {
            title: None,
            text: format!("(unsupported component: {})", node.component.tag()),
        }),
    };
    // Re-check: the replacement itself must be supported, else fall back
    // to a plain note (e.g. a minimal client degrading chart → table →
    // note).
    let retag = replacement.tag();
    node.component = if caps.supports(retag) {
        replacement
    } else {
        Component::Markdown(Markdown {
            title: None,
            text: format!("(unsupported component: {tag})"),
        })
    };
}

/// Best-effort text for a component, used when flattening layouts and
/// for text exports (markdown download, etc.).
pub fn text_summary(node: &UiNode) -> String {
    node_text_fallback(&node.component)
}

/// Best-effort text for a component, used when flattening layouts.
fn node_text_fallback(c: &Component) -> String {
    match c {
        Component::Markdown(m) => m.text.clone(),
        Component::Code(c) => c.code.clone(),
        Component::List(l) => l
            .items
            .iter()
            .map(|i| format!("- {}", i.text))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => format!("(unsupported component: {})", c.tag()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(component: Component) -> UiDocument {
        UiDocument {
            version: UI_PROTOCOL_VERSION,
            id: "turn1-ui_0".to_string(),
            root: UiNode {
                id: "root".to_string(),
                component,
            },
        }
    }

    #[test]
    fn valid_documents_round_trip() {
        let d = doc(Component::Table(Table {
            title: Some("t".to_string()),
            headers: vec!["a".to_string(), "b".to_string()],
            rows: vec![vec!["1".to_string(), "2".to_string()]],
            align: Some(vec![Align::Right, Align::Left]),
        }));
        assert!(d.validate().is_ok());
        let json = serde_json::to_value(&d).unwrap();
        let back: UiDocument = serde_json::from_value(json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn wrong_version_is_rejected() {
        let mut d = doc(Component::Markdown(Markdown {
            title: None,
            text: "hi".to_string(),
        }));
        d.version = 99;
        assert_eq!(d.validate(), Err(UiError::UnsupportedVersion { got: 99 }));
    }

    #[test]
    fn bad_ids_are_rejected() {
        let mut d = doc(Component::Markdown(Markdown {
            title: None,
            text: "hi".to_string(),
        }));
        d.id = "not a slug!".to_string();
        assert!(matches!(d.validate(), Err(UiError::BadId { .. })));
    }

    #[test]
    fn bounds_are_enforced() {
        // Too many table rows.
        let d = doc(Component::Table(Table {
            title: None,
            headers: vec!["a".to_string()],
            rows: vec![vec!["x".to_string()]; MAX_TABLE_ROWS + 1],
            align: None,
        }));
        assert!(matches!(d.validate(), Err(UiError::TooMany { .. })));

        // Chart series/label mismatch.
        let d = doc(Component::Chart(Chart {
            title: None,
            chart_type: ChartType::Bar,
            labels: vec!["a".to_string()],
            series: vec![Series {
                name: "s".to_string(),
                values: vec![1.0, 2.0],
            }],
        }));
        assert!(matches!(d.validate(), Err(UiError::BadChartData { .. })));

        // Non-finite chart value.
        let d = doc(Component::Chart(Chart {
            title: None,
            chart_type: ChartType::Line,
            labels: vec!["a".to_string()],
            series: vec![Series {
                name: "s".to_string(),
                values: vec![f64::NAN],
            }],
        }));
        assert!(matches!(d.validate(), Err(UiError::BadChartData { .. })));

        // Duplicate form field names.
        let field = |name: &str| FormField {
            name: name.to_string(),
            label: "L".to_string(),
            field_type: FieldType::Text,
            required: false,
            default: None,
            placeholder: None,
        };
        let d = doc(Component::Form(Form {
            title: None,
            description: None,
            fields: vec![field("a"), field("a")],
            submit: ActionRef {
                action_id: "go".to_string(),
                label: "Go".to_string(),
                kind: ActionKind::Submit {
                    tool: "test_tool".to_string(),
                },
            },
        }));
        assert!(matches!(d.validate(), Err(UiError::BadField { .. })));
    }

    #[test]
    fn deep_nesting_is_rejected() {
        fn nested(depth: usize) -> UiNode {
            UiNode {
                id: format!("n{depth}"),
                component: if depth == 0 {
                    Component::Markdown(Markdown {
                        title: None,
                        text: "leaf".to_string(),
                    })
                } else {
                    Component::Layout(Layout {
                        kind: LayoutKind::Stack,
                        children: vec![nested(depth - 1)],
                    })
                },
            }
        }
        let d = UiDocument {
            version: UI_PROTOCOL_VERSION,
            id: "t-ui_0".to_string(),
            root: nested(MAX_DEPTH + 2),
        };
        assert!(matches!(d.validate(), Err(UiError::TooDeep { .. })));
    }

    #[test]
    fn unknown_component_tags_degrade_gracefully() {
        let value = json!({
            "version": 1,
            "id": "t-ui_0",
            "root": {
                "id": "root",
                "type": "hologram",
                "text": "from the future"
            }
        });
        // Strict parse fails...
        assert!(serde_json::from_value::<UiDocument>(value.clone()).is_err());
        // ...but the lenient path degrades the subtree and names the tag.
        let (doc, unknown) = UiDocument::from_value_lenient(&value).unwrap();
        assert_eq!(unknown, vec!["hologram".to_string()]);
        assert!(doc.validate().is_ok());
        match &doc.root.component {
            Component::Markdown(m) => assert!(m.text.contains("hologram")),
            other => panic!("expected markdown fallback, got {:?}", other.tag()),
        }
    }

    #[test]
    fn degrade_for_minimal_client() {
        let d = doc(Component::Chart(Chart {
            title: Some("rev".to_string()),
            chart_type: ChartType::Bar,
            labels: vec!["a".to_string(), "b".to_string()],
            series: vec![Series {
                name: "s".to_string(),
                values: vec![1.0, 2.0],
            }],
        }));
        let degraded = degrade_for(&d, &ClientCaps::minimal());
        // Chart -> table -> (minimal has no table) -> markdown note.
        match &degraded.root.component {
            Component::Markdown(m) => assert!(m.text.contains("unsupported")),
            other => panic!("expected markdown fallback, got {:?}", other.tag()),
        }

        // A client that supports tables keeps the table step.
        let caps = ClientCaps {
            supported: vec!["table".to_string(), "markdown".to_string()],
            max_version: UI_PROTOCOL_VERSION,
        };
        let degraded = degrade_for(&d, &caps);
        assert_eq!(degraded.root.component.tag(), "table");
    }

    #[test]
    fn newer_version_degrades_to_a_note() {
        let mut d = doc(Component::Markdown(Markdown {
            title: None,
            text: "hi".to_string(),
        }));
        d.version = 99;
        let degraded = degrade_for(&d, &ClientCaps::full());
        match &degraded.root.component {
            Component::Markdown(m) => assert!(m.text.contains("v99")),
            other => panic!("expected note, got {:?}", other.tag()),
        }
    }

    fn form_doc() -> UiDocument {
        doc(Component::Form(Form {
            title: None,
            description: None,
            fields: vec![
                FormField {
                    name: "name".to_string(),
                    label: "Name".to_string(),
                    field_type: FieldType::Text,
                    required: true,
                    default: None,
                    placeholder: None,
                },
                FormField {
                    name: "count".to_string(),
                    label: "Count".to_string(),
                    field_type: FieldType::Number,
                    required: false,
                    default: None,
                    placeholder: None,
                },
                FormField {
                    name: "color".to_string(),
                    label: "Color".to_string(),
                    field_type: FieldType::Select {
                        options: vec!["red".to_string(), "blue".to_string()],
                    },
                    required: false,
                    default: None,
                    placeholder: None,
                },
            ],
            submit: ActionRef {
                action_id: "submit".to_string(),
                label: "Submit".to_string(),
                kind: ActionKind::Submit {
                    tool: "test_tool".to_string(),
                },
            },
        }))
    }

    fn submission(params: serde_json::Value) -> UiActionSubmission {
        UiActionSubmission {
            revision: 0,
            ui_id: "turn1-ui_0".to_string(),
            node_id: "root".to_string(),
            action_id: "submit".to_string(),
            params: params.as_object().unwrap().clone(),
            nonce: "n1".to_string(),
        }
    }

    #[test]
    fn valid_form_submission_passes() {
        let d = form_doc();
        assert!(d.validate().is_ok());
        let s = submission(json!({"name": "ada", "count": 3, "color": "red"}));
        assert!(s.validate_against(&d).is_ok());
        // Optional fields may be omitted.
        let s = submission(json!({"name": "ada"}));
        assert!(s.validate_against(&d).is_ok());
    }

    #[test]
    fn invalid_submissions_are_rejected() {
        let d = form_doc();
        // Missing required field.
        let s = submission(json!({"count": 3}));
        assert!(matches!(
            s.validate_against(&d),
            Err(UiError::BadParams { .. })
        ));
        // Unknown field.
        let s = submission(json!({"name": "ada", "evil": "x"}));
        assert!(matches!(
            s.validate_against(&d),
            Err(UiError::BadParams { .. })
        ));
        // Wrong type.
        let s = submission(json!({"name": "ada", "count": "many"}));
        assert!(matches!(
            s.validate_against(&d),
            Err(UiError::BadParams { .. })
        ));
        // Select outside options.
        let s = submission(json!({"name": "ada", "color": "green"}));
        assert!(matches!(
            s.validate_against(&d),
            Err(UiError::BadParams { .. })
        ));
        // Wrong action id.
        let mut s = submission(json!({"name": "ada"}));
        s.action_id = "nope".to_string();
        assert!(matches!(
            s.validate_against(&d),
            Err(UiError::UnknownAction { .. })
        ));
        // Empty nonce.
        let mut s = submission(json!({"name": "ada"}));
        s.nonce = String::new();
        assert_eq!(s.validate_against(&d), Err(UiError::BadNonce));
    }

    #[test]
    fn local_actions_never_reach_the_runtime() {
        let d = doc(Component::Layout(Layout {
            kind: LayoutKind::Tabs,
            children: vec![],
        }));
        let s = UiActionSubmission {
            revision: 0,
            ui_id: "turn1-ui_0".to_string(),
            node_id: "root".to_string(),
            action_id: "tab".to_string(),
            params: Default::default(),
            nonce: "n1".to_string(),
        };
        // No declared actions on a layout at all.
        assert!(matches!(
            s.validate_against(&d),
            Err(UiError::UnknownAction { .. })
        ));
    }
}
