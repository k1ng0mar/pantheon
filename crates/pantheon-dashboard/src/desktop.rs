//! Bot Screen (Phase A — watch): live desktop watch for the CUA-driven
//! desktop.
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
//!   taken_since, last_activity}`. Config-only plus a lock-free read of
//!   the takeover lease file; never fails.
//! * `GET /api/desktop/stream?fps=<1-10>` → **WebSocket**. One
//!   `{"type":"ready",...}` text frame, then binary PNG/JPEG frames at the
//!   requested rate (default 2). Capture failures arrive as
//!   `{"type":"error","message":...}` text frames; the stream keeps going.
//!
//! Frames go to the human viewer only and never enter model context.
//! Human takeover (lease, input, release) is Phase B and lives in the
//! same module; the lease file format it uses is defined in
//! `pantheon_runtime::desktop_takeover`.

use crate::{
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
/// Screenshot is all Phase A (watch) needs; the input tools are
/// discovered here so the capability map is validated up front, and
/// Phase B (takeover/input) reads them.
#[derive(Debug)]
#[allow(dead_code)] // input-tool fields are read by Phase B
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
