# Camofox (Camoufox) Research — Programmatic Interface & Pantheon Integration Shape

**Date:** 2026-09-29
**Source-of-truth:** upstream repo [daijro/camoufox](https://github.com/daijro/camoufox), PyPI `camoufox` wrapper, community CLIs/MCP servers observed via web search.
**Note:** Docs point to camoufox.com; the `/docs/` path on camoufox.com 404s (site restructure), so findings below come from the GitHub README, the Nix packaging derivation (maximoffua/camoufox-nix), and third-party READMEs that quote official usage. Ambiguities are marked.

## 1. What Camofox is (identity & naming gotchas)

- **Camoufox** (official spelling, github.com/daijro/camoufox) is a **patched Firefox fork** engineered for anti-detect browsing / web scraping / AI agents. Firefox-based, **not** Chromium.
- A separate npm ecosystem product called **"Camofox"** exists (`camofox-cli`, `camofox-browser`, `camofox-mcp` — jo-inc/camofox-browser lineage). These are *Node servers/CLIs that wrap Camoufox as an "anti-detection browser server"*, not the browser itself. In docs and community writing the two names get conflated constantly. **Recommendation:** use the name "Camoufox" internally and in config IDs to match upstream; treat "camofox-*" npm packages as downstream wrappers.
- The **official PyPI package is `camoufox`** (not `camofox`). The npm JS wrapper is **`camoufox-js`**. A fork `cloverlabs-camoufox` adds GeoIP-by-default and prerelease hardware-spoofing builds.

## 2. How Camoufox is driven programmatically

### Primary (official): Python launcher API over Playwright

Install and fetch:

```bash
pip install "camoufox[geoip]"       # [geoip] enables proxy-IP-derived timezone/locale/geolocation
python -m camoufox fetch           # one-time download of the patched Firefox binary (~100–150 MB)
```

Minimal launch examples (from community docs quoting official usage):

```python
# Sync
from camoufox.sync_api import Camoufox

with Camoufox(headless=True) as browser:
    page = browser.new_page()
    page.goto("https://example.com")
    print(page.title())
```

```python
# Async
import asyncio
from camoufox.async_api import AsyncCamoufox

async def main():
    async with AsyncCamoufox(headless=True) as browser:
        page = await browser.new_page()
        await page.goto("https://example.com")
        print(await page.title())

asyncio.run(main())
```

- The returned `browser`/`page` are **standard Playwright objects** — the full Playwright API works (`click`, `fill`, `wait_for_selector`, etc.).
- Protocol detail: Playwright normally drives Firefox via its **Juggler protocol**, not CDP. Camoufox patches Juggler to operate on an isolated copy of the page so the real page never observes the automation channel. **Upstream explicitly frames this as "sidestepping CDP detection entirely"** — there is no CDP transport for Firefox-Juggler; `navigator.webdriver`-style and CDP-channel probes fail against it differently than against Chromium CDP libraries. (Firefox does support CDP now, but Camoufox's Playwright bindings use Juggler, and its stealth patches target Juggler/Firefox internals.)
- There is **no official standalone CLI** in the upstream Python package (the CLI-ish surfaces — `camoufox-browser`, `camoufox-mcp`, `camoufox-playwright-mcp` — are all third-party).

### Secondary: JS/TS wrapper (`camoufox-js`, experimental Apify port)

```bash
npm install playwright@1.60.0 camoufox-js   # first run auto-fetches the binary into node_modules
```

Same Playwright API surface as the Python interface.

### Secondary: agent-style CLIs (third-party)

- `camoufox-browser` (pip: `pip install camoufox-browser`, then `camoufox-browser install` or `python -m camoufox fetch`) — an `agent-browser`-style CLI with a background daemon, accessibility-snapshot refs, and an optional MCP server. Subcommand shape: `camoufox-browser open <url>`, `snapshot`, `click <ref>`, `fill <ref> <text>`, `screenshot`, `eval`, `close`.
- These are thin wrappers over the official Python package.

### Is there a raw "browser executable" mode?

The fetched binary is a real Firefox; a Nix derivation confirms it can be run headless directly (`camoufox --headless --screenshot <out> <url>`). But without the launcher the fingerprint injection (BrowserForge presets, Juggler patches' config) doesn't happen — driving the bare binary loses the anti-detect value. Upstream did not document an `executable_path`-style Playwright channel hookup.

## 3. Key launch options (fingerprint/automation config)

Passed to `Camoufox(...)` / `AsyncCamoufox(...)`:

| Option | Effect |
|---|---|
| `headless` | `True`, `False`, or `"virtual"` (Xvfb wrapper on Linux; "true headless is patched to look headed", per upstream) |
| `os` | `"windows"`, `"macos"`, `"linux"` (or list to randomize) — drives fonts/navigator consistency |
| `humanize` | `True` or float seconds — human-like cursor movement between actions |
| `geoip` | `True` (needs `[geoip]` extra) — derives timezone/locale/geolocation from proxy IP |
| `proxy` | `{"server": ..., "username": ..., "password": ...}` |
| `locale` / `timezone` | explicit overrides, e.g. `"ko-KR"`, `"Asia/Seoul"` |
| `fingerprint_preset` | `True` → BrowserForge-backed real presets (recommended on recent versions) |
| `addons` | list of Firefox addon `.xpi` paths to load |
| `block_images` / `block_webrtc` | perf / IP-leak prevention toggles |
| `exclude_addons` / `fonts` | further fingerprint shaping |

Unset properties are auto-filled from BrowserForge fingerprints (statistically realistic, internally consistent). Guidance from community docs: prefer leaving things unset over inventing inconsistent values.

## 4. Install requirements & Linux headless feasibility

- **Python:** `pip install camoufox[geoip]` + `python -m camoufox fetch` (~100–150 MB one-time download). The PyPI wrapper itself is small; the binary lands under the package's data dir (`~/.cache` / venv site-packages — exact location not re-verified here).
- **System deps (Ubuntu/Debian):** `libgtk-3-0 libx11-xcb1 libasound2` (plus transitive X libs). No xvfb needed for `headless=True`; `headless="virtual"` or headed needs `xvfb` (`apt install xvfb`) or a real display.
- **Playwright version pinning gotcha (2026):** community reports pin `playwright@1.60.0` for the JS wrapper — Playwright 1.61+ sends a `viewport.isMobile` field that Camoufox's bundled Firefox rejects on `newPage()`, breaking page opens. For the Python path this means: let the `camoufox` package's own dependency pins win; **do not** force-upgrade Playwright around it.
- **First-launch cost:** fetching and first launch take minutes; subsequent launches are fast.
- **Binary size:** ~100–150 MB download; the Nix derivation's release archive is ~660 MB (that's the uncompressed full Firefox tree — normal).

## 5. Gotchas: license, version, maturity

- **License:** browser source is **MPL-2.0** (it's a Firefox fork). The PyPI **wrapper itself is MIT**. MPL-2.0 is a file-level copyleft: embedding/modifying Camoufox files triggers source-disclosure of the *modified files*; merely launching the binary from Pantheon does not infect Pantheon's own code. No redistribution is planned, so this is low-risk — but note it.
- **Maturity:** upstream README carries an explicit **"under development, may not be suitable for stable production use"** warning. Community evidence (Chromium-vs-Camoufox benchmarks, Nix packaging churn, the playwright pin breakage) suggests API churn is real. Treat as an opt-in backend, not the default.
- **Upstream docs URL churn:** camoufox.com restructured; README defers to the site but deep doc paths 404. Pin behaviors against the GitHub repo + PyPI release notes rather than site URLs.
- **Ethics/ToS note:** community tooling around Camoufox is explicit that automated access must be permitted by the target site's terms — same policy posture Pantheon already needs for any browsing backend.

## 6. Recommended integration shape for Pantheon (Rust runtime)

### Recommendation: **subprocess-driven Python shim** — i.e. a `CamofoxBackend` in `crates/pantheon-web/src/browser/` implemented as a managed subprocess, mirroring the existing `gsd-browser` CLI / `@playwright/cli` backends. Do **not** pursue CDP or a Playwright channel.

Rationale:

1. **Established pattern:** `backend.rs`'s trait seam plus `subprocess.rs` already support CLI-subprocess backends with versioned session state (GSD default) and a stateful JSON-over-stdio fallback (`@playwright/cli`). A Camoufox backend slots into `registry::BackendKind` the same way.
2. **No CDP:** Camoufox's Playwright driver uses the patched **Juggler protocol**, not CDP. The raw-`chromiumoxide` CDP backend cannot talk to it. Even a generic websocket-CDP attach would bypass the fingerprint-injection path (which happens in the Python launcher's profile/config setup), defeating the purpose.
3. **No Rust-native path:** there is no Rust Playwright client that supports Firefox-Juggler-with-Camoufox-patches; writing a custom Juggler wire protocol in Rust would be re-implementing the Python launcher's job (profile generation, BrowserForge preset injection, Juggler prefs) — high risk, high churn against an "under development" upstream.
4. **Two concrete subprocess shapes**, in order of preference:
   - **(a) Managed Python shim (recommended):** Pantheon ships/embeds a small Python script (or vendored module) that imports `camoufox.sync_api`, holds one long-lived `Camoufox` browser, and speaks the same JSON-over-stdio command envelope the `@playwright/cli` backend uses (`snapshot`, `navigate`, `click`/`fill` by ref or selector, `screenshot`, `eval`, `close`). This reuses the existing `subprocess.rs` session lifecycle machinery almost verbatim and gives full fingerprint-config control (`os`, `humanize`, `geoip`, `proxy`, `fingerprint_preset`) via `[browser.camofox]` config keys.
   - **(b) Third-party CLI `camoufox-browser`:** agent-browser-style CLI with daemon + snapshot refs; least code in Rust (GSD-shape invocation: `camoufox-browser --json <cmd>`-style or plain args), but adds an npm/PyPI third-party dependency outside upstream's control, and its command surface is less aligned with Pantheon's tool schema. Viable fallback if (a) is deemed too much Python to vendor.
5. **Lifecycle notes:** browser fetch (`python -m camoufox fetch`) is a first-run network cost; the registry should surface a clear error with the install hint when the binary is missing (the GSD backend already has this "missing binary → install hint" pattern). Headless default (`headless=True`); `headless="virtual"` only when `xvfb` is present; headed requires a display and should be an explicit opt-in. Fingerprint config maps 1:1 to `[browser.camofox]` keys.

### Minimal launch example in the recommended shape (Python shim sketch)

```python
# pantheon-side shim (conceptual; reads JSON commands on stdin, writes JSON on stdout)
import json, sys
from camoufox.sync_api import Camoufox

def main():
    cfg = json.loads(sys.argv[1])  # {"headless": True, "os": "windows", "humanize": True, ...}
    with Camoufox(**cfg) as browser:
        page = browser.new_page()
        for line in sys.stdin:
            cmd = json.loads(line)
            if cmd["op"] == "goto":
                page.goto(cmd["url"]); out = {"ok": True, "title": page.title()}
            elif cmd["op"] == "snapshot":
                out = {"ok": True, "aria": page.accessibility.snapshot() if hasattr(page.accessibility, "snapshot") else None}
            elif cmd["op"] == "screenshot":
                page.screenshot(path=cmd["path"]); out = {"ok": True}
            elif cmd["op"] == "eval":
                out = {"ok": True, "result": page.evaluate(cmd["js"])}
            elif cmd["op"] == "close":
                break
            sys.stdout.write(json.dumps(out) + "\n"); sys.stdout.flush()

main()
```

(Exact command vocabulary should mirror the existing `@playwright/cli` backend's JSON contract so `tools.rs` needs no new surface — the backend translates.)

## 7. Local verification in this environment

- `python3 -c "import camoufox"` → **ModuleNotFoundError** (not installed).
- `which camoufox` / `which camofox` → **not found**; `pip show camoufox|camofox` → **not found**.
- **Not installed, and I did not install it** (per task constraint: no heavy install).
- Feasibility assessment: **likely installable here** — Ubuntu 24.04 LTS, Python 3.12, Node v24 present, and the GTK/X/audio shared libraries Firefox needs (`libgtk-3`, `libX11-xcb`, `libasound`, `libXcomposite`, `libXdamage`, `libdbus-1`) are all present in `ldconfig`. What remains untested: the ~100–150 MB `python -m camoufox fetch` download (network egress to GitHub releases), first-launch, and whether any *missing* transitive dep would need `apt` (requires root/sudo — not attempted). Headless mode should work without xvfb; I could not verify any of this without the install, and **I did not launch or test a real binary**.
- No Rust code was modified; nothing committed.

## 8. Open questions / ambiguities

- Exact on-disk location of the fetched binary and the launcher's env-var overrides (useful for a "find binary" check in the registry) — not pinned from docs; the Nix derivation reads `version.json`/`properties.json` next to the binary, which the Python package writes.
- Whether `AsyncCamoufox` supports a remote-Juggler/WebSocket attach mode for attaching to an already-running Camoufox (there is no evidence of a CDP-style `--remote-debugging-port` equivalent) — assume **spawn-only** until proven otherwise.
- GeoIP extra's data source/licensing (MaxMind DB?) — worth a look before shipping `geoip: true` as default.
