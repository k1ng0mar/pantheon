# Browser Backend Verification - 2026-09-29

Live/blocked status for each of the seven browser backends. Sections 1-6
verified by the browser-backends subagent; section 7 (Camoufox) by the
camofox worker. Environment: this Linux VM, Chrome at
`/opt/meta-chromium/chrome` (Chromium 152.0.7977.82), gsd-browser 0.1.24 at
`~/workspace/bin/gsd-browser`.

## 1. GSD Browser - ✅ live-verified

Binary: `~/workspace/bin/gsd-browser`, `gsd-browser 0.1.24`.

Direct daemon launch through `--browser-path` does not work in this
root/headless environment (first: root without `--no-sandbox`; then: a
no-sandbox wrapper needs a display; headless wrapper path wedged).
Workaround that works: launch Chrome manually with
`--remote-debugging-port` and attach GSD with `--cdp-url
http://127.0.0.1:<port>`.

Verified command by command (exit codes + JSON shapes checked):

| Command | Result |
|---|---|
| `navigate data:text/html,...` | ✅ returns settle/state; external `https://example.com` returned `ERR_EMPTY_RESPONSE` (environment networking, not the binary) |
| `snapshot` | ✅ returns `version` + `refs` keyed `e1`, `e2`; interaction input uses `@vN:eM` |
| `click-ref @v2:e2` | ✅ |
| `extract --schema '{"properties": {...}}'` | ✅ returns `data`, `fieldCount`, `multiple`, `scope`; a **bare property map fails** with `schema must have a 'properties' object` - Pantheon normalizes |
| `act --intent primary_cta` | ✅ fixed intent vocabulary confirmed; returned candidate with score `0.35` (no minimum threshold - the approval gate) |
| `wait-for --condition text_visible --value Verify` | ✅ returns `met: true` |
| `daemon stop` | ✅ prints `Daemon stopped.`, exit 0 |
| failure shape | ⚠️ some failures print top-level `{"error": ...}` on stdout **with exit 0** - `SubprocessBackend::invoke` detects this and returns `BrowserError::Failed` |

Replays as an in-crate ignored test:
`PANTHEON_LIVE_GSD_BIN=... PANTHEON_LIVE_CHROME_BIN=... cargo test -p pantheon-web -- --ignored live_gsd_browser_contract`.

## 2. `chromiumoxide` 0.9 - ✅ live-verified

Pinned `=0.9.1`. Live test drives real Chromium 152 (newer than the crate's
protocol era) through Pantheon's CDP driver:

- `navigate`, `snapshot` (versioned `@vN:eM` refs over the AX tree),
  `extract` (accepts the canonical `{"properties": ...}` envelope
  unwrapped by the driver), `wait-for` (`delay` parses ms; `text_visible`
  resolves; unsupported conditions return `UnsupportedCommand`)
- **Schema drift check passes**: Chrome 152 emits CDP events unknown to the
  0.9.1 protocol; they deserialize as `CdpEvent::Other` and the driver is
  unaffected (source-verified in `chromiumoxide_cdp-0.9.1/src/cdp.rs`, then
  proven live)
- `click-ref` on a stale ref fails with `BrowserError::StaleRef`;
  `act-instruction` fails with `BrowserError::UnsupportedCommand`

In-crate ignored test:
`PANTHEON_LIVE_CHROME_BIN=/opt/meta-chromium/chrome cargo test -p pantheon-web -- --ignored live_native_backend_drives_chrome` - **passes**.

## 3. Steel - ⛔ blocked (no API key)

Implementation compiles (`steel-rs = 0.1`): `POST {base}/v1/sessions` →
CDP URL → Pantheon's CDP driver. Self-host URL without a key is allowed;
the key is appended as `?apiKey=` only when non-empty; secret values never
appear in error strings (covered by
`secret_values_never_appear_in_errors`). Live verification is **blocked**:
no `STEEL_API_KEY` was available on 2026-09-29. Unblock: set the secret in
the broker, then run a session against the cloud or a local
`docker run ghcr.io/steel-dev/steel-browser`.

## 4. Browserbase - ⛔ blocked (no API key)

Implementation compiles: `POST /v1/sessions` (key + project id) → raw CDP
`wss://` URL → Pantheon's CDP driver. Registration fails fast without both
values. Live verification is **blocked**: no `BROWSERBASE_API_KEY` /
project id on 2026-09-29.

## 5. Lightpanda - ⛔ blocked (no binary/server)

Implementation compiles with the reduced extraction-only surface
(`navigate`, `extract`, `page-source`, `screenshot`) and binary-launch
arguments matching Lightpanda's Chromium-style flags (binary mode audited
2026-09-29: flags are correct per the pre-existing implementation; the
`serve`-mode lifecycle could not be exercised). Live verification is
**blocked**: no Lightpanda binary or running server was available on
2026-09-29.

## 6. `@playwright/cli` - ⚠️ partially verified

Verified live via `npx -y @playwright/cli --help` on 2026-09-29:
`playwright-cli -s=<session> --json <command>...`; `snapshot` obtains refs;
`click`/`fill`/`hover` accept ref or unique selector; `type <text>` types
into the focused element; `eval <func> [target]`; `screenshot --filename
--type <png|jpeg|webp>`; `close` exists. The subprocess wrapper maps
canonical argv onto this vocabulary. **Not yet done**: no real Playwright
browser session has been launched in this environment; `--json` output
shape for `eval` is assumed raw, and `wait_for` assumes a raw boolean.

## 7. Camoufox - ✅ live-verified (2026-09-29)

Implementation: `CamofoxBackend` (`crates/pantheon-web/src/browser/camofox.rs`)
plus the embedded Python shim (`src/browser/shims/camofox_shim.py`),
registered as `BackendKind::Camofox` (`[browser] backend = "camofox"`;
`"camoufox"` also parses). One long-lived shim process per Pantheon
session over JSON-over-stdio; the shim speaks the canonical command
vocabulary, `wait-for` is polled Rust-side through the shim's `eval`
(sharing `wait_plan` with the Playwright backend), and
`act`/`act-instruction` are unsupported (same as Playwright).

### Install (this VM, 2026-09-29) - what was actually run

- `python3 -m pip install "camoufox[geoip]"` → refused: PEP 668
  externally-managed environment. Retried with
  `python3 -m pip install --break-system-packages "camoufox[geoip]"`
  → **success**: camoufox 0.5.6 (+ playwright 1.62.0, browserforge,
  geoip2/maxminddb, ...).
- `python3 -m camoufox fetch` → release **152.0.4-beta.31** (prerelease;
  fetch prompts `Continue with prerelease installation?`). First attempt
  died at 534/664 MB with `[Errno 28] No space left on device`: the
  downloader buffers the zip through `tempfile`, and /tmp is a 510 MB
  tmpfs. Retried with `TMPDIR=$HOME/tmp-camoufox` → **success**;
  extracted to `~/.cache/camoufox/browsers/official/152.0.4-beta.31-3a7958c8/`
  (1.2 GB). Note: the real download was **663.5 MB**, not the ~150 MB the
  research doc estimated - update any sizing guidance accordingly.
- Headless smoke test (raw launcher API): `Camoufox(headless=True)` →
  `new_page()` → `goto(data:...)` → `page.title()` = `SmokeOK`,
  `evaluate` = `hi` → **SMOKE PASS** (~15 s including first-launch uBlock
  Origin addon download).

### End-to-end through the Rust backend - ✅ passes

`PANTHEON_LIVE_CAMOFOX=1 cargo test -p pantheon-web -- --ignored live_camofox_shim`
**passes** (5.1 s): shim materialization → spawn → `{"ready": true}`
handshake → `navigate` → `snapshot` (versioned `@vN:eM` refs) → `eval` →
`close`. A manual stdin/stdout session against the shim confirmed the same
envelope plus clean `close`.

### API note (caught live)

camoufox 0.5.6's `Camoufox` is a `PlaywrightContextManager`: `__enter__()`
**returns** the Playwright `Browser` - calling `browser.new_page()` on the
`Camoufox` object itself raises `AttributeError`. The shim keeps both the
launcher (for `__exit__`) and the entered browser object. The research
doc's `with Camoufox(...) as browser:` sketch was correct; the first shim
draft discarded the return value and was fixed.

### Graceful degradation - verified

With the `camoufox` import forced to fail, the shim prints
`{"ready": false, "error": "<install hint>", "kind": "missing"}` and exits
3 - no traceback. The Rust backend maps this to
`BrowserError::BinaryMissing` carrying `CAMOFOX_INSTALL_INSTRUCTIONS`;
registration itself stays keyless and lazy (no subprocess until first
`invoke`).

## Test-plan note

The task's test plan assumed a `pantheon-eval` crate for
subprocess/network/timing tests. That crate does not exist in the 19-crate
workspace, and this agent did not create a 20th crate unilaterally. Live
tests are in-crate, `#[ignore]`d, and env-gated (see above). If Umar wants a
dedicated eval crate, that's his call.
