//! Bot Screen: live desktop watch + human takeover for the CUA-driven desktop.
//!
//! Mirrors `browser.rs`'s take-control pattern, but the "backend" is the
//! CUA driver (`cua-driver`, trycua/cua) the agent loop drives as an MCP
//! server. The dashboard owns its own [`McpManager`] with just the driver
//! spec — the same sanctioned path the agent loop uses (same approval
//! store, same tool projection), in this process. No second ad-hoc driver
//! invocation: everything goes through the manager.
//!
//! One deliberate difference from the agent path: screenshots go through
//! [`McpManager::call_tool`] directly, not the projected registry tools.
//! The registry projection keeps only text
//! (`pantheon_mcp::result_text` drops image blocks); the stream needs the
//! raw image bytes.
//!
//! Endpoints (all under `/api/`, so dashboard token auth applies):
//!
//! * `GET /api/desktop/status` → `{enabled, driver, controller, viewers,
//!   taken_since, last_activity}`. Config-only; never fails.
//! * `GET /api/desktop/stream?fps=<1-10>` → **WebSocket**. One
//!   `{"type":"ready",...}` text frame, then binary PNG/JPEG frames at the
//!   requested rate (default 2). Capture failures arrive as
//!   `{"type":"error","message":...}` text frames; the stream keeps going.
//!   Client → server messages are ignored - input goes through
//!   `POST /api/desktop/input`.
//! * `POST /api/desktop/takeover` → `{lease_id, expires_in_ms}`. Fails 409
//!   while a live lease is held (or 503 when computer-use is disabled).
//! * `POST /api/desktop/heartbeat` → `{lease_id}` keeps a lease alive.
//! * `POST /api/desktop/input` → `{lease_id, action, ...}`; closed action
//!   set: `click` (x, y), `type` (text), `press` (key), `scroll`
//!   (dx, dy, x?, y?), `drag` (x1, y1, x2, y2). Coordinates clamp to the
//!   reported screen size when known.
//! * `POST /api/desktop/release` → `{lease_id}` → `{ok, summary}`; the
//!   summary also lands in the agent's next computer-use tool result (see
//!   `pantheon_runtime::desktop_takeover`).
//!
//! Takeover state lives in `<data_dir>/desktop-takeover.json` so the
//! dashboard and the agent loop — possibly different processes — see the
//! same lease. While a lease is live, the agent loop's `PreToolCall` path
//! parks computer-use tools with "human has control". Frames go to the
//! human viewer only and never enter model context.

use crate::{
    bad_json, body_json,
    browser::{ts_rfc3339, ws_text, HeadReplay},
    err_json, json_ok, App,
};
use pantheon_gateway::http::{Request, Response};
use pantheon_runtime::desktop_takeover as takeover;
use std::collections::HashSet;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_FPS: u32 = 2;
const MAX_FPS: u32 = 10;
const DRIVER_SERVER: &str = pantheon_runtime::computer::CUA_DRIVER_SERVER;

/// Raw MCP tool names on the driver, discovered at runtime from the
/// projected registry (the `mcp_cua-driver_` prefix is stripped).
#[derive(Debug)]
pub(crate) struct DesktopTools {
    screenshot: String,
    click: Option<String>,
    type_text: Option<String>,
    press: Option<String>,
    scroll: Option<String>,
    drag: Option<String>,
    screen_size: Option<String>,
}

/// The dashboard's own driver handle: one MCP manager (one driver
/// process) per dashboard process, shared by all viewers/inputs.
pub struct DesktopDriver {
    manager: Arc<pantheon_mcp::manager::McpManager>,
    tools: DesktopTools,
    screen: Mutex<Option<(u32, u32)>>,
    viewers: Mutex<HashSet<String>>,
}

/// `[computer_use]` resolution for the dashboard: the ComputerUse
/// tool-group toggle decides, then the section's driver/binary pins.
fn computer_section(app: &App) -> Result<pantheon_api::config::ComputerUseSection, Response> {
    let cfg = crate::session_factory::load_config(&app.data_dir)
        .map_err(|e| err_json(500, "DESKTOP", &format!("load config: {e}")))?;
    let enablement = cfg
        .as_ref()
        .and_then(|c| c.tools.as_ref())
        .map(pantheon_runtime::tool_config::ToolEnablement::from_section)
        .unwrap_or_default();
    if !enablement.is_enabled(pantheon_api::config::ToolGroup::ComputerUse) {
        return Err(err_json(
            503,
            "COMPUTER_USE_DISABLED",
            "desktop control is disabled ([tools] computer_use = false)",
        ));
    }
    Ok(cfg
        .as_ref()
        .and_then(|c| c.computer_use.clone())
        .unwrap_or_default())
}

/// Lazily-initialized, process-shared driver handle. Slow path (driver
/// spawn + MCP handshake) runs at most twice concurrently; the loser is
/// dropped.
fn driver_for(app: &App) -> Result<Arc<DesktopDriver>, Response> {
    {
        let guard = app.desktop.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = guard.as_ref() {
            return Ok(Arc::clone(d));
        }
    }
    let driver = Arc::new(init_driver(app)?);
    let mut guard = app.desktop.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = guard.as_ref() {
        return Ok(Arc::clone(d));
    }
    *guard = Some(Arc::clone(&driver));
    Ok(driver)
}

fn init_driver(app: &App) -> Result<DesktopDriver, Response> {
    let section = computer_section(app)?;
    let spec = pantheon_runtime::computer::cua_driver_spec(
        section.driver.as_deref(),
        section.binary.as_deref(),
    )
    .ok_or_else(|| {
        err_json(
            503,
            "DRIVER_NOT_FOUND",
            "cua-driver not found on PATH; install it or set [computer_use] binary",
        )
    })?;
    let manager = Arc::new(pantheon_mcp::manager::McpManager::new(app.data_dir.clone()));
    manager.configure(vec![spec]);
    let mut reg = pantheon_tools::tools::ToolRegistry::new();
    let report = manager.register_server_tools(&mut reg, DRIVER_SERVER, None);
    if report.pending_approval.iter().any(|n| n == DRIVER_SERVER) {
        return Err(err_json(
            503,
            "DRIVER_UNAPPROVED",
            "cua-driver needs approval: pantheon mcp approve cua-driver",
        ));
    }
    if let Some((_, reason)) = report.skipped.first() {
        return Err(err_json(
            502,
            "DRIVER_FAILED",
            &format!("cua-driver unavailable: {reason}"),
        ));
    }
    let tools = resolve_tools(&reg.names()).map_err(|e| err_json(502, "DRIVER_TOOLS", &e))?;
    let driver = DesktopDriver {
        manager,
        tools,
        screen: Mutex::new(None),
        viewers: Mutex::new(HashSet::new()),
    };
    seed_screen_size(&driver);
    Ok(driver)
}

/// Recover raw driver tool names from the projected
/// `mcp_cua-driver_*` registry names. Screenshot is required; the rest
/// are optional and checked per action.
fn resolve_tools(names: &[String]) -> Result<DesktopTools, String> {
    let prefix = format!("mcp_{DRIVER_SERVER}_");
    let bare: Vec<&str> = names
        .iter()
        .filter_map(|n| n.strip_prefix(&prefix))
        .collect();
    let pick = |cands: &[&str]| -> Option<String> {
        cands
            .iter()
            .find_map(|c| bare.iter().find(|b| *b == c))
            .map(|s| s.to_string())
    };
    let screenshot = pick(&[
        "screenshot",
        "computer_screenshot",
        "take_screenshot",
        "capture_screenshot",
    ])
    .ok_or_else(|| {
        format!(
            "driver exposes no screenshot tool (projected tools: {})",
            bare.join(", ")
        )
    })?;
    Ok(DesktopTools {
        screenshot,
        click: pick(&["click", "computer_click"]),
        type_text: pick(&["type_text", "type", "computer_type"]),
        press: pick(&["press_key", "press", "computer_press_key"]),
        scroll: pick(&["scroll", "computer_scroll"]),
        drag: pick(&["drag", "computer_drag"]),
        screen_size: pick(&["get_screen_size", "computer_get_screen_size"]),
    })
}

/// Best-effort screen-size seed from the driver's own tool (it knows its
/// display). PNG frames update the cache on every capture anyway.
fn seed_screen_size(driver: &DesktopDriver) {
    let Some(tool) = driver.tools.screen_size.as_deref() else {
        return;
    };
    let Ok(value) = driver
        .manager
        .call_tool(DRIVER_SERVER, tool, &serde_json::json!({}))
    else {
        return;
    };
    let size = value
        .get("width")
        .and_then(|v| v.as_u64())
        .zip(value.get("height").and_then(|v| v.as_u64()))
        .map(|(w, h)| (w as u32, h as u32))
        .or_else(|| {
            // Fallback: first two plausible numbers in the JSON text.
            let text = value.to_string();
            let mut nums = text
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|s| s.parse::<u32>().ok())
                .filter(|&n| n >= 100);
            nums.next().zip(nums.next())
        });
    if let Some((w, h)) = size.filter(|&(w, h)| w > 0 && h > 0) {
        *driver.screen.lock().unwrap_or_else(|e| e.into_inner()) = Some((w, h));
    }
}

/// Standard base64 (not url-safe). Hand-rolled: the workspace has no
/// base64 crate and this is the only place that needs one.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: char) -> Result<u8, String> {
        match c {
            'A'..='Z' => Ok(c as u8 - b'A'),
            'a'..='z' => Ok(c as u8 - b'a' + 26),
            '0'..='9' => Ok(c as u8 - b'0' + 52),
            '+' => Ok(62),
            '/' => Ok(63),
            _ => Err(format!("invalid base64 character {c:?}")),
        }
    }
    let mut out = Vec::new();
    let mut quad = [0u8; 4];
    let mut qlen = 0usize;
    let mut pad = 0u32;
    for c in s.chars() {
        if c == '=' {
            pad += 1;
            if pad > 2 {
                return Err("too much base64 padding".to_string());
            }
            quad[qlen] = 0;
            qlen += 1;
        } else {
            if pad > 0 {
                return Err("base64 data after padding".to_string());
            }
            quad[qlen] = val(c)?;
            qlen += 1;
        }
        if qlen == 4 {
            let n = ((quad[0] as u32) << 18)
                | ((quad[1] as u32) << 12)
                | ((quad[2] as u32) << 6)
                | quad[3] as u32;
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
            qlen = 0;
            pad = 0;
        }
    }
    if qlen != 0 {
        return Err("truncated base64 input".to_string());
    }
    Ok(out)
}

/// (width, height) from PNG IHDR or JPEG SOF; None when unrecognized.
/// Bounds-checked throughout: malformed images degrade to None, never
/// panic.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let be_u32 = |i: usize| -> Option<u32> {
        Some(u32::from_be_bytes([
            *bytes.get(i)?,
            *bytes.get(i + 1)?,
            *bytes.get(i + 2)?,
            *bytes.get(i + 3)?,
        ]))
    };
    // PNG: 8-byte signature, then length + "IHDR" + width + height.
    // Only 24 bytes are read; a real file always carries the IHDR CRC
    // and more, but the dimensions are valid with just these.
    if bytes.len() >= 24
        && bytes.get(..8) == Some(&b"\x89PNG\r\n\x1a\n"[..])
        && bytes.get(12..16) == Some(&b"IHDR"[..])
    {
        let (w, h) = (be_u32(16)?, be_u32(20)?);
        return (w > 0 && h > 0).then_some((w, h));
    }
    // JPEG: SOI, then segments; SOF0-SOF3 carry dimensions.
    if bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut i = 2usize;
        while i + 9 < bytes.len() {
            if bytes[i] != 0xFF {
                break;
            }
            let marker = bytes[i + 1];
            if (0xC0..=0xC3).contains(&marker) {
                let h = u16::from_be_bytes([*bytes.get(i + 5)?, *bytes.get(i + 6)?]) as u32;
                let w = u16::from_be_bytes([*bytes.get(i + 7)?, *bytes.get(i + 8)?]) as u32;
                return (w > 0 && h > 0).then_some((w, h));
            }
            if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([*bytes.get(i + 2)?, *bytes.get(i + 3)?]) as usize;
            if len < 2 {
                break;
            }
            i += 2 + len;
        }
    }
    None
}

/// Pull PNG/JPEG bytes out of an MCP image content block. The registry
/// projection drops image blocks (`result_text` keeps text only), which
/// is why screenshots go through `call_tool` directly.
fn extract_image_bytes(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    let blocks = value
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or("screenshot result has no content blocks")?;
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) != Some("image") {
            continue;
        }
        let data = b
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or("image block has no data")?;
        // Accept raw base64 or a data: URI.
        let b64 = data
            .strip_prefix("data:")
            .and_then(|r| r.split_once(";base64,"))
            .map(|(_, d)| d)
            .unwrap_or(data);
        let bytes =
            base64_decode(b64).map_err(|e| format!("image data is not valid base64: {e}"))?;
        if image_dimensions(&bytes).is_none() {
            return Err("screenshot is not a PNG or JPEG image".to_string());
        }
        return Ok(bytes);
    }
    Err("screenshot result contained no image block".to_string())
}

/// One screenshot → validated image bytes. Updates the cached screen
/// size from the frame itself.
fn capture_png(driver: &DesktopDriver) -> Result<Vec<u8>, String> {
    let value = driver
        .manager
        .call_tool(
            DRIVER_SERVER,
            &driver.tools.screenshot,
            &serde_json::json!({}),
        )
        .map_err(|e| format!("screenshot tool failed: {e}"))?;
    let bytes = extract_image_bytes(&value)?;
    if let Some((w, h)) = image_dimensions(&bytes) {
        *driver.screen.lock().unwrap_or_else(|e| e.into_inner()) = Some((w, h));
    }
    Ok(bytes)
}

/// Clamp a point to the reported screen. Unknown screen size passes
/// through (the driver validates); the clamp is a client-bug backstop,
/// not the security boundary — token auth and the closed action set
/// are.
fn clamp_xy(x: f64, y: f64, screen: Option<(u32, u32)>) -> (i64, i64) {
    match screen {
        Some((w, h)) => (
            (x.round() as i64).clamp(0, w as i64),
            (y.round() as i64).clamp(0, h as i64),
        ),
        None => (x.round() as i64, y.round() as i64),
    }
}

/// Map one take-control action to (raw driver tool, args, audit detail).
/// Pure function — unit-tested. `detail` is display-safe by construction:
/// typed text never lands in it (it may contain credentials), mirroring
/// `browser.rs`'s `takeover_activity`.
pub(crate) fn input_plan(
    action: &str,
    body: &serde_json::Value,
    screen: Option<(u32, u32)>,
    tools: &DesktopTools,
) -> Result<(String, serde_json::Value, String), String> {
    let num = |key: &str| -> Result<f64, String> {
        body.get(key)
            .and_then(|v| v.as_f64())
            .filter(|f| f.is_finite())
            .ok_or_else(|| format!("'{key}' must be a finite number"))
    };
    let str_field = |key: &str| -> Result<String, String> {
        body.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("'{key}' must be a non-empty string"))
    };
    let need = |tool: Option<&String>, what: &str| -> Result<String, String> {
        tool.cloned()
            .ok_or_else(|| format!("driver exposes no {what} tool"))
    };
    match action {
        "click" => {
            let (x, y) = clamp_xy(num("x")?, num("y")?, screen);
            Ok((
                need(tools.click.as_ref(), "click")?,
                serde_json::json!({"x": x, "y": y}),
                format!("click at ({x}, {y})"),
            ))
        }
        "type" => {
            let text = str_field("text")?;
            Ok((
                need(tools.type_text.as_ref(), "type")?,
                serde_json::json!({"text": text}),
                "typing into a field".to_string(),
            ))
        }
        "press" => {
            let key = str_field("key")?;
            Ok((
                need(tools.press.as_ref(), "press")?,
                serde_json::json!({"key": key}),
                format!("pressed {key}"),
            ))
        }
        "scroll" => {
            let dx = body
                .get("dx")
                .and_then(|v| v.as_f64())
                .filter(|f| f.is_finite())
                .unwrap_or(0.0);
            let dy = body
                .get("dy")
                .and_then(|v| v.as_f64())
                .filter(|f| f.is_finite())
                .unwrap_or(0.0);
            let (x, y) = match (
                body.get("x").and_then(|v| v.as_f64()),
                body.get("y").and_then(|v| v.as_f64()),
            ) {
                (Some(x), Some(y)) => clamp_xy(x, y, screen),
                // Anchor at screen center when known; drivers scroll
                // relative deltas, so the anchor only needs to be sane.
                _ => screen
                    .map(|(w, h)| (w as i64 / 2, h as i64 / 2))
                    .unwrap_or((0, 0)),
            };
            Ok((
                need(tools.scroll.as_ref(), "scroll")?,
                serde_json::json!({"x": x, "y": y, "dx": dx, "dy": dy}),
                format!("scroll ({dx}, {dy})"),
            ))
        }
        "drag" => {
            let (x1, y1) = clamp_xy(num("x1")?, num("y1")?, screen);
            let (x2, y2) = clamp_xy(num("x2")?, num("y2")?, screen);
            Ok((
                need(tools.drag.as_ref(), "drag")?,
                serde_json::json!({"x1": x1, "y1": y1, "x2": x2, "y2": y2}),
                format!("drag ({x1}, {y1}) to ({x2}, {y2})"),
            ))
        }
        other => Err(format!(
            "unknown action '{other}': click | type | press | scroll | drag"
        )),
    }
}

/// `GET /api/desktop/status` → `{enabled, driver, controller, viewers,
/// taken_since, last_activity}`. Config-only; never fails.
pub fn status(app: &App, _req: &Request) -> Response {
    let (enabled, driver) = match computer_section(app) {
        Ok(s) => (true, s.driver.unwrap_or_else(|| "cua-driver".to_string())),
        Err(_) => (false, "cua-driver".to_string()),
    };
    let (controller, taken_since, last_activity) =
        match std::fs::read_to_string(takeover::lease_path(&app.data_dir)) {
            Ok(raw) => match serde_json::from_str::<takeover::TakeoverFile>(&raw) {
                Ok(file) => {
                    let live = file
                        .lease
                        .as_ref()
                        .filter(|l| takeover::now_ms() < l.expires_at_ms);
                    let last_activity = live
                        .and_then(|l| l.actions.last())
                        .map(|a| {
                            serde_json::json!({
                                "action": a.action,
                                "detail": a.detail,
                                "at": ts_rfc3339(a.at_ms),
                            })
                        })
                        .or_else(|| {
                            file.last_release.as_ref().map(|r| {
                                serde_json::json!({
                                    "action": "release",
                                    "detail": r.summary,
                                    "at": ts_rfc3339(r.at_ms),
                                })
                            })
                        })
                        .unwrap_or(serde_json::Value::Null);
                    (
                        live.map(|l| l.holder.clone()),
                        live.map(|l| ts_rfc3339(l.acquired_at_ms)),
                        last_activity,
                    )
                }
                Err(_) => (None, None, serde_json::Value::Null),
            },
            Err(_) => (None, None, serde_json::Value::Null),
        };
    let viewers = app
        .desktop
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|d| d.viewers.lock().unwrap_or_else(|e| e.into_inner()).len())
        .unwrap_or(0);
    json_ok(serde_json::json!({
        "enabled": enabled,
        "driver": driver,
        "controller": controller,
        "viewers": viewers,
        "taken_since": taken_since,
        "last_activity": last_activity,
    }))
}

/// `POST /api/desktop/takeover` → `{lease_id, expires_in_ms}`.
/// 409 while a live, heartbeating lease is held; a lease whose heartbeat
/// lapsed >30s is claimable.
pub fn takeover(app: &App, _req: &Request) -> Response {
    if let Err(r) = computer_section(app) {
        return r;
    }
    let now = takeover::now_ms();
    let res = takeover::with_takeover_locked(&app.data_dir, |file| {
        if !takeover::claimable(file) {
            let holder = file
                .lease
                .as_ref()
                .map(|l| l.holder.clone())
                .unwrap_or_default();
            let since = file.lease.as_ref().map(|l| l.acquired_at_ms).unwrap_or(0);
            return Err((holder, since));
        }
        let lease = takeover::TakeoverLease {
            lease_id: takeover::new_lease_id(),
            holder: "dashboard".to_string(),
            acquired_at_ms: now,
            expires_at_ms: now + takeover::LEASE_TTL_MS,
            last_heartbeat_ms: now,
            actions: Vec::new(),
        };
        let id = lease.lease_id.clone();
        file.lease = Some(lease);
        // A fresh takeover supersedes any release note: the new holder's
        // actions are what the next tool result should narrate.
        file.last_release = None;
        Ok(id)
    });
    match res {
        Err(e) => err_json(
            500,
            "DESKTOP_LOCK",
            &format!("takeover state unavailable: {e}"),
        ),
        Ok(Err((holder, since))) => err_json(
            409,
            "DESKTOP_CONTROLLED",
            &format!(
                "desktop is already controlled by {holder} (since {})",
                ts_rfc3339(since)
            ),
        ),
        Ok(Ok(id)) => json_ok(serde_json::json!({
            "lease_id": id,
            "expires_in_ms": takeover::LEASE_TTL_MS,
        })),
    }
}

/// `POST /api/desktop/heartbeat` `{lease_id}` → `{ok, expires_in_ms}`.
pub fn heartbeat(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let lease_id = match body
        .get("lease_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        None => return bad_json("lease_id is required"),
    };
    let now = takeover::now_ms();
    let res = takeover::with_takeover_locked(&app.data_dir, |file| match file.lease.as_mut() {
        Some(l) if l.lease_id == lease_id && now < l.expires_at_ms => {
            l.last_heartbeat_ms = now;
            l.expires_at_ms = now + takeover::LEASE_TTL_MS;
            Ok(())
        }
        _ => Err(()),
    });
    match res {
        Err(e) => err_json(
            500,
            "DESKTOP_LOCK",
            &format!("takeover state unavailable: {e}"),
        ),
        Ok(Err(())) => err_json(
            409,
            "DESKTOP_NO_LEASE",
            "no live takeover for this lease_id",
        ),
        Ok(Ok(())) => json_ok(serde_json::json!({
            "ok": true,
            "expires_in_ms": takeover::LEASE_TTL_MS,
        })),
    }
}

/// `POST /api/desktop/input` `{lease_id, action, ...}` — one human input
/// action into the desktop. Requires a live, matching lease.
pub fn input(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let lease_id = match body
        .get("lease_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        None => return bad_json("lease_id is required"),
    };
    let action = match body
        .get("action")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(a) => a.to_string(),
        None => return bad_json("action is required"),
    };
    // Verify the lease BEFORE touching the driver: a wrong lease_id must
    // not spawn anything.
    let live = takeover::with_takeover_locked(
        &app.data_dir,
        |file| matches!(&file.lease, Some(l) if l.lease_id == lease_id && takeover::now_ms() < l.expires_at_ms),
    );
    match live {
        Err(e) => {
            return err_json(
                500,
                "DESKTOP_LOCK",
                &format!("takeover state unavailable: {e}"),
            )
        }
        Ok(false) => {
            return err_json(
                409,
                "DESKTOP_NO_LEASE",
                "no live takeover for this lease_id",
            )
        }
        Ok(true) => {}
    }
    let driver = match driver_for(app) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let screen = *driver.screen.lock().unwrap_or_else(|e| e.into_inner());
    let (tool, args, detail) = match input_plan(&action, &body, screen, &driver.tools) {
        Ok(t) => t,
        Err(e) => return bad_json(&e),
    };
    let result = match driver.manager.call_tool(DRIVER_SERVER, &tool, &args) {
        Ok(v) => pantheon_mcp::result_text(&v),
        Err(e) => return err_json(502, "DESKTOP_INPUT", &format!("driver input failed: {e}")),
    };
    // Record the action for the audit trail and release summary.
    // Best-effort: the input already happened; a dead ledger must not
    // fail it. Re-verify the lease still matches before appending.
    let now = takeover::now_ms();
    let _ = takeover::with_takeover_locked(&app.data_dir, |file| {
        if let Some(l) = file.lease.as_mut() {
            if l.lease_id == lease_id {
                l.actions.push(takeover::TakeoverAction {
                    action: action.clone(),
                    detail: detail.clone(),
                    at_ms: now,
                });
            }
        }
    });
    // Keep the result short; it may echo coordinates but never secrets
    // (typed text is never in `detail`, and the result is the driver's
    // terse acknowledgement).
    let short = {
        let line = result.lines().next().unwrap_or("ok");
        const MAX: usize = 300;
        if line.len() > MAX {
            format!("{}...", &line[..MAX])
        } else {
            line.to_string()
        }
    };
    json_ok(serde_json::json!({
        "ok": true,
        "action": action,
        "detail": detail,
        "result": short,
    }))
}

/// `POST /api/desktop/release` `{lease_id}` → `{ok, summary}`. Ends the
/// lease; the summary is returned here, kept for the agent's next
/// computer-use tool result, and the lease file retains it as the audit
/// record.
pub fn release(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let lease_id = match body
        .get("lease_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        None => return bad_json("lease_id is required"),
    };
    // Distinguish corrupt state (409: take over again) from simply no
    // lease (404/409): with_takeover_locked reads corrupt as default,
    // which would hide the difference.
    match std::fs::read_to_string(takeover::lease_path(&app.data_dir)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return err_json(409, "DESKTOP_NO_LEASE", "no live takeover to release")
        }
        Ok(raw) => {
            if serde_json::from_str::<takeover::TakeoverFile>(&raw).is_err() {
                return err_json(
                    409,
                    "DESKTOP_STATE_CORRUPT",
                    "takeover state unreadable; take over again to establish a fresh lease",
                );
            }
        }
        Err(e) => {
            return err_json(
                500,
                "DESKTOP_LOCK",
                &format!("takeover state unavailable: {e}"),
            )
        }
    }
    let now = takeover::now_ms();
    let res = takeover::with_takeover_locked(&app.data_dir, |file| match file.lease.take() {
        Some(lease) if lease.lease_id == lease_id => {
            let summary = takeover::release_summary(&lease);
            file.last_release = Some(takeover::ReleaseRecord {
                summary: summary.clone(),
                at_ms: now,
                consumed: false,
            });
            Ok(summary)
        }
        Some(lease) => {
            file.lease = Some(lease);
            Err("lease_id does not match the live takeover".to_string())
        }
        None => Err("no live takeover to release".to_string()),
    });
    match res {
        Err(e) => err_json(
            500,
            "DESKTOP_LOCK",
            &format!("takeover state unavailable: {e}"),
        ),
        Ok(Err(msg)) => err_json(409, "DESKTOP_NO_LEASE", &msg),
        Ok(Ok(summary)) => json_ok(serde_json::json!({ "ok": true, "summary": summary })),
    }
}

/// Replays the already-consumed HTTP request head, then delegates to the
/// socket, so `tungstenite::accept` can parse the WS handshake itself —
/// the same trick `browser.rs` (and live voice) uses.
fn ws_head(req: &Request) -> Vec<u8> {
    let qs: Vec<String> = req.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let target = if qs.is_empty() {
        req.path.clone()
    } else {
        format!("{}?{}", req.path, qs.join("&"))
    };
    let mut head = format!("{} {target} HTTP/1.1\r\n", req.method);
    for (k, v) in &req.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    head.into_bytes()
}

/// `GET /api/desktop/stream?fps=<1-10>` — WebSocket upgrade; binary
/// PNG/JPEG frames of the live desktop.
pub fn stream(app: &App, req: &Request) -> Response {
    let driver = match driver_for(app) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let fps = req
        .query
        .get("fps")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_FPS)
        .clamp(1, MAX_FPS);
    let head = ws_head(req);
    let viewer = pantheon_runtime::new_run_id();
    Response::Takeover {
        run: Box::new(move |stream| serve_stream(stream, head, driver, fps, viewer)),
    }
}

/// The stream loop: capture → PNG → binary frame, at `fps`. Runs on the
/// connection thread until the client disconnects. Capture failures are
/// reported as text frames; the loop survives them (the driver may be
/// starting or the display sleeping).
fn serve_stream(
    stream: TcpStream,
    head: Vec<u8>,
    driver: Arc<DesktopDriver>,
    fps: u32,
    viewer: String,
) {
    driver
        .viewers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(viewer.clone());
    let replay = HeadReplay {
        head,
        pos: 0,
        stream,
    };
    let mut ws = match tungstenite::accept(replay) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("desktop stream: websocket handshake failed: {e}");
            driver
                .viewers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&viewer);
            return;
        }
    };
    ws_text(&mut ws, &format!("{{\"type\":\"ready\",\"fps\":{fps}}}"));
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let _ = ws.get_mut().stream_mut().set_read_timeout(Some(interval));
    loop {
        match capture_png(&driver) {
            Ok(bytes) => {
                if ws.send(tungstenite::Message::Binary(bytes.into())).is_err() {
                    break;
                }
            }
            Err(e) => ws_text(
                &mut ws,
                &format!(
                    "{{\"type\":\"error\",\"message\":{}}}",
                    serde_json::to_string(&e).unwrap_or_else(|_| "\"capture failed\"".into())
                ),
            ),
        }
        match ws.read() {
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {} // client → server messages are ignored by design
            Err(tungstenite::Error::ConnectionClosed) => break,
            Err(tungstenite::Error::AlreadyClosed) => break,
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
    }
    driver
        .viewers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&viewer);
    let _ = ws.close(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tools() -> DesktopTools {
        DesktopTools {
            screenshot: "screenshot".to_string(),
            click: Some("click".to_string()),
            type_text: Some("type_text".to_string()),
            press: Some("press_key".to_string()),
            scroll: Some("scroll".to_string()),
            drag: Some("drag".to_string()),
            screen_size: Some("get_screen_size".to_string()),
        }
    }

    #[test]
    fn unknown_action_rejected_with_valid_set() {
        let err = input_plan("rm", &serde_json::json!({}), None, &test_tools())
            .expect_err("unknown action");
        assert!(
            err.contains("click | type | press | scroll | drag"),
            "{err}"
        );
    }

    #[test]
    fn click_maps_and_clamps() {
        let (tool, args, detail) = input_plan(
            "click",
            &serde_json::json!({"x": 5000.0, "y": -10.0}),
            Some((1920, 1080)),
            &test_tools(),
        )
        .expect("click");
        assert_eq!(tool, "click");
        assert_eq!(args, serde_json::json!({"x": 1920, "y": 0}));
        assert_eq!(detail, "click at (1920, 0)");
    }

    #[test]
    fn click_without_screen_passes_through() {
        let (_, args, _) = input_plan(
            "click",
            &serde_json::json!({"x": 100.0, "y": 200.0}),
            None,
            &test_tools(),
        )
        .expect("click");
        assert_eq!(args, serde_json::json!({"x": 100, "y": 200}));
    }

    #[test]
    fn click_rejects_non_numbers() {
        input_plan(
            "click",
            &serde_json::json!({"x": "a", "y": 1.0}),
            None,
            &test_tools(),
        )
        .expect_err("non-numeric x");
    }

    #[test]
    fn type_never_echoes_text_in_detail() {
        let (tool, args, detail) = input_plan(
            "type",
            &serde_json::json!({"text": "s3cr3t-p@ssw0rd"}),
            None,
            &test_tools(),
        )
        .expect("type");
        assert_eq!(tool, "type_text");
        assert_eq!(args, serde_json::json!({"text": "s3cr3t-p@ssw0rd"}));
        assert_eq!(detail, "typing into a field");
        assert!(
            !detail.contains("s3cr3t"),
            "typed text must never land in the audit detail"
        );
        input_plan(
            "type",
            &serde_json::json!({"text": "   "}),
            None,
            &test_tools(),
        )
        .expect_err("blank text");
    }

    #[test]
    fn missing_driver_tool_is_a_clear_error() {
        let mut tools = test_tools();
        tools.drag = None;
        let err = input_plan(
            "drag",
            &serde_json::json!({"x1": 1.0, "y1": 1.0, "x2": 2.0, "y2": 2.0}),
            None,
            &tools,
        )
        .expect_err("missing drag tool");
        assert!(err.contains("no drag tool"), "{err}");
    }

    #[test]
    fn resolve_tools_requires_screenshot() {
        let names = vec!["mcp_cua-driver_click".to_string()];
        let err = resolve_tools(&names).expect_err("no screenshot tool");
        assert!(err.contains("no screenshot tool"), "{err}");
    }

    #[test]
    fn resolve_tools_picks_bare_and_computer_names() {
        let names = vec![
            "mcp_cua-driver_screenshot".to_string(),
            "mcp_cua-driver_computer_click".to_string(),
            "mcp_cua-driver_type_text".to_string(),
        ];
        let tools = resolve_tools(&names).expect("resolve");
        assert_eq!(tools.screenshot, "screenshot");
        assert_eq!(tools.click.as_deref(), Some("computer_click"));
        assert_eq!(tools.type_text.as_deref(), Some("type_text"));
        assert_eq!(tools.press, None);
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TQ==").unwrap(), b"M");
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("TQ==TQ==").unwrap(), b"MM");
        base64_decode("TWFu!").expect_err("bad char");
        base64_decode("TWF").expect_err("truncated");
        base64_decode("TQ=TQ==").expect_err("data after padding");
    }

    /// Minimal valid PNG: signature + IHDR (width 800, height 600).
    fn tiny_png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&800u32.to_be_bytes());
        v.extend_from_slice(&600u32.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth etc. (content irrelevant)
        v
    }

    #[test]
    fn png_dimensions_parsed() {
        assert_eq!(image_dimensions(&tiny_png()), Some((800, 600)));
        assert_eq!(image_dimensions(b"not an image"), None);
        assert_eq!(image_dimensions(&b"\x89PNG\r\n\x1a\n"[..]), None);
    }

    /// Minimal JPEG: SOI, APP0, SOF0 with 480x640.
    fn tiny_jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8]; // SOI
        v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]); // APP0, len 16
        v.extend_from_slice(&[0u8; 14]); // APP0 payload
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B]); // SOF0, len 11
        v.push(8); // precision
        v.extend_from_slice(&480u16.to_be_bytes()); // height
        v.extend_from_slice(&640u16.to_be_bytes()); // width
        v.extend_from_slice(&[0u8; 6]);
        v
    }

    #[test]
    fn jpeg_dimensions_parsed() {
        assert_eq!(image_dimensions(&tiny_jpeg()), Some((640, 480)));
    }

    #[test]
    fn extract_image_bytes_handles_data_uri_and_raw() {
        let png_b64 = {
            // base64 of tiny_png, computed here to avoid a literal
            const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let bytes = tiny_png();
            let mut s = String::new();
            for chunk in bytes.chunks(3) {
                let mut n = 0u32;
                for (i, b) in chunk.iter().enumerate() {
                    n |= (*b as u32) << (16 - 8 * i);
                }
                for i in 0..4 {
                    if i <= chunk.len() {
                        s.push(ALPH[((n >> (18 - 6 * i)) & 63) as usize] as char);
                    } else {
                        s.push('=');
                    }
                }
            }
            s
        };
        let raw = serde_json::json!({
            "content": [{"type": "image", "mimeType": "image/png", "data": png_b64}]
        });
        assert_eq!(extract_image_bytes(&raw).unwrap(), tiny_png());
        let uri = serde_json::json!({
            "content": [{"type": "image", "data": format!("data:image/png;base64,{png_b64}")}]
        });
        assert_eq!(extract_image_bytes(&uri).unwrap(), tiny_png());
        let no_image = serde_json::json!({"content": [{"type": "text", "text": "hi"}]});
        extract_image_bytes(&no_image).expect_err("no image block");
    }
}
